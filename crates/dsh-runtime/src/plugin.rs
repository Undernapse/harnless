//! Plugin lifecycle and dependency-ordered activation.

use std::collections::HashSet;

use crate::context::Context;
use crate::error::{Result, RuntimeError};
use crate::fiber::{Fiber, FiberState};
use crate::service::ServiceId;

/// Stable operational identity for a plugin instance.
#[derive(Debug, Clone, PartialEq, Eq, Hash)]
pub struct PluginId(String);

impl PluginId {
    pub fn new(value: impl Into<String>) -> Self {
        Self(value.into())
    }

    pub fn as_str(&self) -> &str {
        &self.0
    }
}

/// A mountable unit of behavior.
pub trait Plugin: Send {
    fn id(&self) -> PluginId;

    /// Services that must exist before [`apply`](Plugin::apply) runs.
    fn inject(&self) -> Vec<ServiceId> {
        Vec::new()
    }

    /// Install this plugin's registrations into its fiber-owned context.
    fn apply(&mut self, context: &Context) -> Result<()>;
}

/// An active plugin and the fiber owning its reversible effects.
pub struct MountedPlugin {
    id: PluginId,
    inject: Vec<ServiceId>,
    provides: Vec<ServiceId>,
    fiber: Fiber,
    _plugin: Box<dyn Plugin>,
}

impl MountedPlugin {
    pub fn id(&self) -> &PluginId {
        &self.id
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
        let id = plugin.id();
        if self.pending.iter().any(|pending| pending.id() == id)
            || self.mounted.iter().any(|mounted| mounted.id == id)
        {
            return Err(RuntimeError::duplicate_plugin());
        }
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
            let id = plugin.id();
            let inject = plugin.inject();
            let services_before = self.context.local_service_ids();
            let fiber = Fiber::pending();
            fiber.set_state(FiberState::Loading);
            let plugin_context = self.context.owned_by(fiber.clone());
            if let Err(error) = plugin.apply(&plugin_context) {
                fiber.set_state(FiberState::Failed);
                fiber.dispose();
                return Err(error);
            }
            fiber.set_state(FiberState::Active);
            let provides = self
                .context
                .local_service_ids()
                .difference(&services_before)
                .copied()
                .collect();
            self.mounted.push(MountedPlugin {
                id,
                inject,
                provides,
                fiber,
                _plugin: plugin,
            });
        }
    }

    /// Unload a plugin and all consumers of services that disappear with it.
    pub fn unload(&mut self, id: &PluginId) -> bool {
        let Some(target) = self.mounted.iter().position(|plugin| &plugin.id == id) else {
            return false;
        };

        let mut remove = HashSet::from([self.mounted[target].id.clone()]);
        let mut disappearing: HashSet<ServiceId> =
            self.mounted[target].provides.iter().copied().collect();
        loop {
            let mut changed = false;
            for plugin in &self.mounted {
                if !remove.contains(&plugin.id)
                    && plugin
                        .inject
                        .iter()
                        .any(|service| disappearing.contains(service))
                {
                    remove.insert(plugin.id.clone());
                    disappearing.extend(plugin.provides.iter().copied());
                    changed = true;
                }
            }
            if !changed {
                break;
            }
        }

        // Reverse activation order keeps dependencies alive while consumers unwind.
        for index in (0..self.mounted.len()).rev() {
            if remove.contains(&self.mounted[index].id) {
                let plugin = self.mounted.remove(index);
                plugin.dispose();
            }
        }
        true
    }
}

impl Drop for PluginLoader {
    fn drop(&mut self) {
        while let Some(plugin) = self.mounted.pop() {
            plugin.dispose();
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicBool, Ordering};
    use std::sync::Arc;

    use crate::{service_id, Disposer};

    struct Provider(Option<Disposer>);

    impl Plugin for Provider {
        fn id(&self) -> PluginId {
            PluginId::new("provider")
        }

        fn apply(&mut self, context: &Context) -> Result<()> {
            self.0 = Some(context.provide(42_u32)?);
            Ok(())
        }
    }

    struct Consumer {
        applied: Arc<AtomicBool>,
        disposed: Arc<AtomicBool>,
        effect: Option<Disposer>,
    }

    impl Plugin for Consumer {
        fn id(&self) -> PluginId {
            PluginId::new("consumer")
        }

        fn inject(&self) -> Vec<ServiceId> {
            vec![service_id::<u32>()]
        }

        fn apply(&mut self, context: &Context) -> Result<()> {
            assert_eq!(context.get::<u32>().as_deref(), Some(&42));
            self.applied.store(true, Ordering::SeqCst);
            let disposed = self.disposed.clone();
            self.effect = Some(context.effect(move || {
                Some(Box::new(move || {
                    disposed.store(true, Ordering::SeqCst);
                }))
            })?);
            Ok(())
        }
    }

    fn consumer(applied: Arc<AtomicBool>, disposed: Arc<AtomicBool>) -> Consumer {
        Consumer {
            applied,
            disposed,
            effect: None,
        }
    }

    #[test]
    fn consumer_waits_until_its_injected_service_exists() {
        let applied = Arc::new(AtomicBool::new(false));
        let mut loader = PluginLoader::new(Context::new());
        loader
            .mount(Box::new(consumer(
                applied.clone(),
                Arc::new(AtomicBool::new(false)),
            )))
            .unwrap();
        assert_eq!(loader.pending_len(), 1);

        loader.mount(Box::new(Provider(None))).unwrap();
        assert!(applied.load(Ordering::SeqCst));
        assert_eq!(loader.mounted()[0].id().as_str(), "provider");
        assert_eq!(loader.mounted()[1].id().as_str(), "consumer");
    }

    #[test]
    fn unloading_a_provider_unloads_consumers_first() {
        let disposed = Arc::new(AtomicBool::new(false));
        let mut loader = PluginLoader::new(Context::new());
        loader.mount(Box::new(Provider(None))).unwrap();
        loader
            .mount(Box::new(consumer(
                Arc::new(AtomicBool::new(false)),
                disposed.clone(),
            )))
            .unwrap();

        assert!(loader.unload(&PluginId::new("provider")));
        assert!(disposed.load(Ordering::SeqCst));
        assert!(loader.context().get::<u32>().is_none());
        assert!(loader.mounted().is_empty());
    }
}
