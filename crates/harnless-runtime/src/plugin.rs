//! Plugin definition and inject-driven activation.
//!
//! Mirroring the reference registry semantics (research 02): a plugin
//! declares the services it requires via [`Plugin::inject`]; the loader
//! activates it only once every required service exists, so load order is
//! expressed as service requirements and never a hand-maintained boot
//! sequence.
//!
//! One fiber per mounted plugin (decision 03-Q8): unloading a plugin unwinds
//! exactly its own tools, listeners, schemas, and prompt sections.

use std::any::TypeId;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::context::Context;
use crate::error::Result;
use crate::fiber::{Fiber, FiberState};

/// A plugin: an installable unit contributing services, listeners, and
/// registrations to a context.
pub trait Plugin: Send + Sync + 'static {
    /// Display name used for fiber diagnostics and logger names.
    fn name(&self) -> &str;

    /// The services this plugin requires, as stable type keys.
    ///
    /// The loader stalls activation until every required service is present
    /// on the context (inject-driven load order, decision 03-Q4).
    fn inject(&self) -> &'static [TypeId] {
        &[]
    }

    /// Apply the plugin: install services, listeners, and effects on `ctx`.
    ///
    /// Every registration made inside `apply` attaches to the plugin's fiber
    /// and unwinds when the plugin is unloaded. Config travels as a JSON
    /// value at this layer; typed validation belongs to the caller.
    fn apply(&self, ctx: &Context) -> Result<()>;
}

/// The plugin registry: activates plugins when their injected services
/// become available and owns their fibers.
#[derive(Default)]
pub struct Registry {
    plugins: Mutex<Vec<Mounted>>,
}

struct Mounted {
    #[allow(dead_code)] // retained for identity-based unmount bookkeeping
    plugin: Arc<dyn Plugin>,
    fiber: Arc<Fiber>,
}

impl Registry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// Try to mount `plugin` on `ctx`.
    ///
    /// Fails with code `MISSING_SERVICE` when any injected service is absent
    /// — the caller retries once the dependency mounts (service-requirement
    /// activation, decision 03-Q4). On success the plugin body has run inside
    /// a fresh fiber; return the fiber so the caller can unload it, or null
    /// the plug by dropping the registry handle.
    pub fn mount(&self, ctx: &Context, plugin: Arc<dyn Plugin>) -> Result<Arc<Fiber>> {
        for key in plugin.inject() {
            if !ctx.contains_id(*key) {
                return Err(crate::error::RuntimeError::new(
                    "MISSING_SERVICE",
                    "plugin activation gated on missing service",
                ));
            }
        }

        let fiber = Fiber::pending();
        // The plugin context extends the parent (shares services); the fiber
        // scope is owned by this mount.
        let plugin_ctx = ctx.extend();
        plugin_ctx.set_fiber(fiber.clone());

        fiber.set_state(FiberState::Loading);
        let applied = plugin.apply(&plugin_ctx);
        match applied {
            Ok(()) => {
                fiber.set_state(FiberState::Active);
            }
            Err(err) => {
                // Roll back partial registrations, then surface the failure.
                fiber.set_state(FiberState::Failed);
                fiber.dispose();
                return Err(err);
            }
        }

        self.plugins.lock().push(Mounted {
            plugin,
            fiber: fiber.clone(),
        });
        Ok(fiber)
    }

    /// Unmount a plugin previously mounted here, unwinding exactly its fiber.
    pub fn unmount(&self, fiber: &Arc<Fiber>) {
        let mut plugins = self.plugins.lock();
        if let Some(pos) = plugins.iter().position(|m| Arc::ptr_eq(&m.fiber, fiber)) {
            plugins.remove(pos);
        }
        fiber.dispose();
    }

    /// Number of mounted plugins.
    pub fn len(&self) -> usize {
        self.plugins.lock().len()
    }

    /// Whether no plugin is mounted.
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Names of mounted plugins, in mount order.
    pub fn names(&self) -> Vec<String> {
        self.plugins
            .lock()
            .iter()
            .map(|m| m.plugin.name().to_owned())
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::error::RuntimeError;

    struct ProbeService;

    struct ProvidesProbe;
    impl Plugin for ProvidesProbe {
        fn name(&self) -> &str {
            "provides-probe"
        }
        fn apply(&self, ctx: &Context) -> Result<()> {
            ctx.provide(ProbeService)?;
            Ok(())
        }
    }

    static PROBE_SERVICE_INJECT: [TypeId; 1] = [TypeId::of::<ProbeService>()];

    struct NeedsProbe;
    impl Plugin for NeedsProbe {
        fn name(&self) -> &str {
            "needs-probe"
        }
        fn inject(&self) -> &'static [TypeId] {
            &PROBE_SERVICE_INJECT
        }
        fn apply(&self, _ctx: &Context) -> Result<()> {
            Ok(())
        }
    }

    #[test]
    fn mount_gates_on_missing_service() {
        let registry = Registry::new();
        let ctx = Context::root();
        let err = registry.mount(&ctx, Arc::new(NeedsProbe)).unwrap_err();
        assert_eq!(err.code, "MISSING_SERVICE");
        assert!(registry.is_empty());
    }

    #[test]
    fn mount_runs_apply_and_tracks_fiber() {
        let registry = Registry::new();
        let ctx = Context::root();
        let fiber = registry.mount(&ctx, Arc::new(ProvidesProbe)).unwrap();
        assert!(ctx.has::<ProbeService>());
        assert_eq!(fiber.state(), FiberState::Active);
        assert_eq!(registry.names(), vec!["provides-probe"]);
    }

    #[test]
    fn inject_gate_satisfied_by_earlier_plugin() {
        let registry = Registry::new();
        let ctx = Context::root();
        registry.mount(&ctx, Arc::new(ProvidesProbe)).unwrap();
        let fiber = registry.mount(&ctx, Arc::new(NeedsProbe)).unwrap();
        assert_eq!(fiber.state(), FiberState::Active);
        assert_eq!(registry.len(), 2);
    }

    #[test]
    fn failed_apply_rolls_back_registrations() {
        struct ExplodeThenProvide;
        impl Plugin for ExplodeThenProvide {
            fn name(&self) -> &str {
                "explode"
            }
            fn apply(&self, ctx: &Context) -> Result<()> {
                ctx.provide(ProbeService)?;
                Err(RuntimeError::new("BOOM", "apply exploded"))
            }
        }
        let registry = Registry::new();
        let ctx = Context::root();
        let err = registry
            .mount(&ctx, Arc::new(ExplodeThenProvide))
            .unwrap_err();
        assert_eq!(err.code, "BOOM");
        // The partial registration was unwound with the failed fiber.
        assert!(!ctx.has::<ProbeService>());
        assert!(registry.is_empty());
    }

    #[test]
    fn unmount_disposes_fiber_and_removes_services() {
        let registry = Registry::new();
        let ctx = Context::root();
        let fiber = registry.mount(&ctx, Arc::new(ProvidesProbe)).unwrap();
        assert!(ctx.has::<ProbeService>());
        registry.unmount(&fiber);
        assert!(!ctx.has::<ProbeService>());
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert!(registry.is_empty());
    }
}
