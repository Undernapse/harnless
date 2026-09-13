//! The boot half of the contract: mount a composed document, or say exactly
//! which plugin and stage failed.
//!
//! Mounting is the only place a plugin's opaque `config` is interpreted, and
//! the only place a failure can leave resources behind. Both hazards are
//! handled here:
//!
//! * **Every failure names the plugin row and the stage.** A row whose
//!   `plugin` is not registered fails with `unknown-plugin` naming both the
//!   row id and the plugin name; a factory that refuses its config fails with
//!   `plugin-build-failed` naming the row. Consumers route on the code and
//!   read the name; they never parse prose.
//! * **Partial boot state is disposed.** Rows mount in order; if row *n*
//!   fails, the resources handed to rows `0..n` are [`dispose`](MountGuard::dispose)d
//!   in reverse order before the error is returned. The mount is therefore
//!   all-or-nothing: a failed boot never leaves a live process holding a
//!   terminal, socket, or half-mounted service.

use crate::doc::ConfigDoc;
use crate::error::{ConfigError, Result, Stage};

/// One mounted row's live resource.
///
/// The composition layer never learns what a plugin actually is — a
/// subprocess handle, a filesystem provider, a model adapter all look the
/// same here — it only guarantees that whatever a factory handed back gets
/// disposed when the composition ends or fails.
pub trait MountedResource: Send + Sync {
    /// The row id this resource was mounted for.
    fn id(&self) -> &str;

    /// Release this resource, exactly once.
    ///
    /// Implementations MUST be idempotent: [`MountGuard::dispose`] runs each
    /// disposer in reverse order and a resource may already have been torn
    /// down by its own owner.
    fn dispose(&mut self);

    /// This resource as `dyn Any`.
    ///
    /// The composition layer keeps resources behind `dyn`, so this is the one
    /// sanctioned way for a consumer to recover a concrete handle it knows a
    /// particular plugin's resource carries. It is object-safe (generic
    /// methods are not); [`MountGuard::find`] is the ergonomic form callers
    /// actually use.
    fn as_any(&self) -> &dyn std::any::Any;
}

/// A factory is registered under a plugin *name*; the row's `config` arrives
/// untouched so the plugin owns its own validation. Returning an error is how
/// a plugin rejects a bad config, and the mount attributes it to the row.
pub trait PluginFactory: Send + Sync {
    /// The plugin name this factory is registered under.
    fn plugin(&self) -> &str;

    /// Instantiate the resource for a row carrying `config`.
    ///
    /// The `id` is the row identity, for the resource's own diagnostics and
    /// for [`MountedResource::id`].
    fn build(&self, id: &str, config: &serde_yaml::Value) -> std::result::Result<Box<dyn MountedResource>, String>;
}

/// A factory backed by a closure — the shape most plugins need.
pub struct FnFactory<F, T>
where
    F: Fn(&str, &serde_yaml::Value) -> std::result::Result<T, String> + Send + Sync,
    T: MountedResource + 'static,
{
    plugin: String,
    build: F,
    _marker: std::marker::PhantomData<fn() -> T>,
}

impl<F, T> FnFactory<F, T>
where
    F: Fn(&str, &serde_yaml::Value) -> std::result::Result<T, String> + Send + Sync,
    T: MountedResource + 'static,
{
    /// A factory registered as `plugin`, building `T` with `build`.
    pub fn new(plugin: impl Into<String>, build: F) -> Self {
        Self {
            plugin: plugin.into(),
            build,
            _marker: std::marker::PhantomData,
        }
    }
}

impl<F, T> PluginFactory for FnFactory<F, T>
where
    F: Fn(&str, &serde_yaml::Value) -> std::result::Result<T, String> + Send + Sync,
    T: MountedResource + 'static,
{
    fn plugin(&self) -> &str {
        &self.plugin
    }

    fn build(&self, id: &str, config: &serde_yaml::Value) -> std::result::Result<Box<dyn MountedResource>, String> {
        (self.build)(id, config).map(|resource| Box::new(resource) as Box<dyn MountedResource>)
    }
}

/// The plugin registry the mount stage consults: name → factory.
#[derive(Default)]
pub struct PluginRegistry {
    factories: std::collections::BTreeMap<String, std::sync::Arc<dyn PluginFactory>>,
}

impl PluginRegistry {
    /// An empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Register `factory` under its own plugin name, replacing any previous
    /// registration.
    pub fn register(&mut self, factory: std::sync::Arc<dyn PluginFactory>) -> &mut Self {
        self.factories
            .insert(factory.plugin().to_string(), factory);
        self
    }

    /// Register a named factory built from a closure.
    pub fn register_fn<F, T>(&mut self, plugin: impl Into<String>, build: F) -> &mut Self
    where
        F: Fn(&str, &serde_yaml::Value) -> std::result::Result<T, String> + Send + Sync + 'static,
        T: MountedResource + 'static,
    {
        self.register(std::sync::Arc::new(FnFactory::new(plugin, build)))
    }

    /// The factory registered under `plugin`, if any.
    pub fn get(&self, plugin: &str) -> Option<std::sync::Arc<dyn PluginFactory>> {
        self.factories.get(plugin).cloned()
    }

    /// Registered plugin names, in display order.
    pub fn names(&self) -> Vec<String> {
        self.factories.keys().cloned().collect()
    }
}

/// The resources a successful mount handed back, in mount order.
///
/// The guard is `Debug` by its live row ids, so a failed or leaked
/// composition is diagnosable from a debug print.
#[derive(Default)]
pub struct MountGuard {
    resources: Vec<Box<dyn MountedResource>>,
    disposed: bool,
}

impl core::fmt::Debug for MountGuard {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("MountGuard")
            .field("ids", &self.ids())
            .field("disposed", &self.disposed)
            .finish()
    }
}

impl MountGuard {
    /// An empty guard.
    pub fn new() -> Self {
        Self::default()
    }

    /// Take ownership of a mounted resource.
    pub fn push(&mut self, resource: Box<dyn MountedResource>) {
        self.resources.push(resource);
    }

    /// The mounted row ids, in mount order.
    pub fn ids(&self) -> Vec<String> {
        self.resources.iter().map(|r| r.id().to_string()).collect()
    }

    /// Take ownership of every resource another guard holds.
    ///
    /// Mounting a sub-document yields its own guard; merging is how a caller
    /// that mounts row-by-row keeps one disposal owner, so the outer guard's
    /// reverse-order teardown still unwinds everything. The source guard is
    /// left empty (and therefore inert).
    pub fn absorb(&mut self, other: &mut MountGuard) {
        let taken = std::mem::take(&mut other.resources);
        other.disposed = self.disposed;
        self.resources.extend(taken);
    }

    /// Number of live resources.
    pub fn len(&self) -> usize {
        self.resources.len()
    }

    /// Whether nothing is mounted.
    pub fn is_empty(&self) -> bool {
        self.resources.is_empty()
    }

    /// The first resource that is a `T`, if any.
    ///
    /// This is how a consumer recovers a concrete handle the composition
    /// layer cannot name — the composed model adapter, say — without the
    /// guard leaking ownership of its resources.
    pub fn find<T: Send + Sync + 'static>(&self) -> Option<&T> {
        self.resources
            .iter()
            .find_map(|resource| resource.as_any().downcast_ref::<T>())
    }

    /// Dispose every resource in reverse mount order, exactly once.
    ///
    /// A panicking disposer never breaks teardown: every remaining
    /// resource is still disposed, matching the runtime's fiber-teardown
    /// discipline ("one broken disposer never breaks teardown").
    pub fn dispose(&mut self) {
        if self.disposed {
            return;
        }
        self.disposed = true;
        for resource in self.resources.iter_mut().rev() {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                resource.dispose();
            }));
        }
        self.resources.clear();
    }
}

impl Drop for MountGuard {
    fn drop(&mut self) {
        self.dispose();
    }
}

/// Mount every row of `doc` through `registry`.
///
/// On success the caller owns the [`MountGuard`] and the composition is live;
/// on failure the guard is disposed inside this call and the returned error
/// names the plugin row and stage that broke.
pub fn mount(doc: &ConfigDoc, registry: &PluginRegistry) -> Result<MountGuard> {
    let mut guard = MountGuard::new();
    for row in &doc.rows {
        let factory = registry.get(&row.plugin).ok_or_else(|| {
            ConfigError::for_plugin(
                Stage::Mount,
                "unknown-plugin",
                row.id.clone(),
                format!(
                    "row {:?} names plugin {:?}, which is not registered (known: {})",
                    row.id,
                    row.plugin,
                    if registry.names().is_empty() {
                        "(none)".to_string()
                    } else {
                        registry.names().join(", ")
                    }
                ),
            )
        })?;
        match factory.build(&row.id, &row.config) {
            Ok(resource) => guard.push(resource),
            Err(message) => {
                // Dispose what this mount already built, in reverse order,
                // before surfacing the failure: a failed boot leaves nothing
                // holding a terminal or a socket.
                guard.dispose();
                return Err(ConfigError::for_plugin(
                    Stage::Mount,
                    "plugin-build-failed",
                    row.id.clone(),
                    format!("plugin {:?} failed to mount row {:?}: {message}", row.plugin, row.id),
                ));
            }
        }
    }
    Ok(guard)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::doc::Row;
    use parking_lot::Mutex;
    use std::sync::Arc;

    /// A recorder shared by factories and resources.
    #[derive(Clone, Default)]
    struct Log(Arc<Mutex<Vec<String>>>);

    impl Log {
        fn push(&self, entry: &str) {
            self.0.lock().push(entry.to_string());
        }
        fn events(&self) -> Vec<String> {
            self.0.lock().clone()
        }
    }

    /// A resource that records its disposal.
    struct Probe {
        id: String,
        log: Log,
    }

    impl MountedResource for Probe {
        fn id(&self) -> &str {
            &self.id
        }

        fn as_any(&self) -> &dyn std::any::Any {
            self
        }
        fn dispose(&mut self) {
            self.log.push(&format!("dispose:{}", self.id));
        }
    }

    fn probe_registry(log: Log, fail: Option<&str>) -> PluginRegistry {
        let fail = fail.map(|s| s.to_string());
        let mut registry = PluginRegistry::new();
        registry.register_fn("probe", move |id: &str, _config: &serde_yaml::Value| {
            if Some(id.to_string()) == fail {
                return Err("configured to fail".to_string());
            }
            Ok(Probe {
                id: id.to_string(),
                log: log.clone(),
            })
        });
        registry
    }

    fn doc(rows: Vec<&str>) -> ConfigDoc {
        ConfigDoc {
            name: "t".to_string(),
            rows: rows
                .iter()
                .map(|id| Row::new(*id, "probe"))
                .collect(),
            system_prompt: None,
            model: None,
        }
    }

    #[test]
    fn a_midway_mount_failure_disposes_earlier_resources_in_reverse() {
        let log = Log::default();
        let registry = probe_registry(log.clone(), Some("second"));
        let err = mount(&doc(vec!["first", "second", "third"]), &registry).unwrap_err();
        assert_eq!(err.stage, Stage::Mount);
        assert_eq!(err.code, "plugin-build-failed");
        assert!(err.names_plugin("second"), "must name the failing row");
        assert_eq!(
            log.events(),
            vec!["dispose:first"],
            "earlier resources disposed, later ones never built"
        );
    }

    #[test]
    fn an_unregistered_plugin_names_the_row_and_plugin() {
        let log = Log::default();
        let mut registry = PluginRegistry::new();
        registry.register_fn("other", move |id: &str, _c: &serde_yaml::Value| {
            Ok(Probe {
                id: id.to_string(),
                log: log.clone(),
            })
        });
        let err = mount(&doc(vec!["first"]), &registry).unwrap_err();
        assert_eq!(err.stage, Stage::Mount);
        assert_eq!(err.code, "unknown-plugin");
        assert!(err.names_plugin("first"));
        assert!(err.message.contains("probe"), "{}", err.message);
    }

    #[test]
    fn a_successful_mount_disposes_everything_on_drop() {
        let log = Log::default();
        let registry = probe_registry(log.clone(), None);
        let guard = mount(&doc(vec!["a", "b"]), &registry).unwrap();
        assert_eq!(guard.ids(), vec!["a", "b"]);
        drop(guard);
        assert_eq!(log.events(), vec!["dispose:b", "dispose:a"]);
    }

    #[test]
    fn dispose_is_idempotent() {
        let log = Log::default();
        let registry = probe_registry(log.clone(), None);
        let mut guard = mount(&doc(vec!["a"]), &registry).unwrap();
        guard.dispose();
        guard.dispose();
        drop(guard);
        assert_eq!(log.events(), vec!["dispose:a"]);
    }
}
