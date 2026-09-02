//! Plugin lifecycle and dependency-ordered activation.

use crate::context::Context;
use crate::error::Result;
use crate::fiber::{Fiber, FiberState};
use crate::service::ServiceId;

/// A mountable unit of behavior.
pub trait Plugin: Send {
    /// Human-readable identity used for diagnostics and explicit unloads.
    fn name(&self) -> &str;

    /// Services that must exist before [`apply`](Plugin::apply) runs.
    fn inject(&self) -> Vec<ServiceId> {
        Vec::new()
    }

    /// Install this plugin's registrations into its fiber-owned context.
    fn apply(&mut self, context: &Context) -> Result<()>;
}

/// An active plugin and the fiber owning its reversible effects.
pub struct MountedPlugin {
    name: String,
    fiber: Fiber,
    _plugin: Box<dyn Plugin>,
}

impl MountedPlugin {
    pub fn name(&self) -> &str {
        &self.name
    }

    pub fn state(&self) -> FiberState {
        self.fiber.state()
    }

    pub fn dispose(&self) {
        self.fiber.dispose();
    }
}

impl Drop for MountedPlugin {
    fn drop(&mut self) {
        self.fiber.dispose();
    }
}

/// Activates plugins as soon as every declared service dependency exists.
pub struct PluginLoader {
    context: Context,
    pending: Vec<Box<dyn Plugin>>,
    mounted: Vec<MountedPlugin>,
}

impl PluginLoader {
    pub fn new(context: Context) -> Self {
        Self {
            context,
            pending: Vec::new(),
            mounted: Vec::new(),
        }
    }

    pub fn context(&self) -> &Context {
        &self.context
    }

    pub fn pending_len(&self) -> usize {
        self.pending.len()
    }

    pub fn mounted(&self) -> &[MountedPlugin] {
        &self.mounted
    }

    /// Queue a plugin and activate every plugin whose injections are satisfied.
    pub fn mount(&mut self, plugin: Box<dyn Plugin>) -> Result<()> {
        self.pending.push(plugin);
        self.activate_ready()
    }

    fn activate_ready(&mut self) -> Result<()> {
        loop {
            let Some(index) = self.pending.iter().position(|plugin| {
                plugin
                    .inject()
                    .into_iter()
                    .all(|id| self.context.contains_id(id))
            }) else {
                return Ok(());
            };

            let mut plugin = self.pending.remove(index);
            let fiber = Fiber::pending();
            fiber.set_state(FiberState::Loading);
            let plugin_context = self.context.owned_by(fiber.clone());
            if let Err(error) = plugin.apply(&plugin_context) {
                fiber.set_state(FiberState::Failed);
                fiber.dispose();
                return Err(error);
            }
            fiber.set_state(FiberState::Active);
            self.mounted.push(MountedPlugin {
                name: plugin.name().to_owned(),
                fiber,
                _plugin: plugin,
            });
        }
    }

    /// Unload one named plugin. Its effects unwind before it leaves the loader.
    pub fn unload(&mut self, name: &str) -> bool {
        let Some(index) = self.mounted.iter().position(|plugin| plugin.name == name) else {
            return false;
        };
        let plugin = self.mounted.remove(index);
        plugin.dispose();
        true
    }
}

impl Drop for PluginLoader {
    fn drop(&mut self) {
        // Consumers activated after providers, so reverse activation order keeps
        // dependencies alive while their consumers unwind.
        while let Some(plugin) = self.mounted.pop() {
            plugin.dispose();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::any::TypeId;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use crate::{service_id, Disposer};

    struct Provider {
        registration: Option<Disposer>,
    }

    impl Plugin for Provider {
        fn name(&self) -> &str {
            "provider"
        }

        fn apply(&mut self, context: &Context) -> Result<()> {
            self.registration = Some(context.provide(42_u32)?);
            Ok(())
        }
    }

    struct Consumer(Arc<AtomicBool>);

    impl Plugin for Consumer {
        fn name(&self) -> &str {
            "consumer"
        }

        fn inject(&self) -> Vec<TypeId> {
            vec![service_id::<u32>()]
        }

        fn apply(&mut self, context: &Context) -> Result<()> {
            assert_eq!(context.get::<u32>().as_deref(), Some(&42));
            self.0.store(true, Ordering::SeqCst);
            Ok(())
        }
    }

    #[test]
    fn consumer_waits_until_its_injected_service_exists() {
        let applied = Arc::new(AtomicBool::new(false));
        let mut loader = PluginLoader::new(Context::new());
        loader.mount(Box::new(Consumer(applied.clone()))).unwrap();
        assert_eq!(loader.pending_len(), 1);
        assert!(!applied.load(Ordering::SeqCst));

        loader
            .mount(Box::new(Provider { registration: None }))
            .unwrap();
        assert_eq!(loader.pending_len(), 0);
        assert!(applied.load(Ordering::SeqCst));
        assert_eq!(loader.mounted()[0].name(), "provider");
        assert_eq!(loader.mounted()[1].name(), "consumer");
    }

    #[test]
    fn unloading_a_provider_removes_its_service() {
        let mut loader = PluginLoader::new(Context::new());
        loader
            .mount(Box::new(Provider { registration: None }))
            .unwrap();
        assert!(loader.context().get::<u32>().is_some());
        assert!(loader.unload("provider"));
        assert!(loader.context().get::<u32>().is_none());
    }
}
