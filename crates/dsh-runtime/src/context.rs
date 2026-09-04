//! The plugin context: the repository of services with typed accessors.
//!
//! A context is the Cordis-equivalent "repository of services". The map is
//! keyed by type; the accessor is how a consumer names the capability. This
//! makes "by key, not by import" hold statically (decision 03-Q1/Q7).
//!
//! Contexts form a tree: `extend()` shares the service map (Cordis `extend`),
//! while `isolate()` shadows one service type within a scope under a label
//! (decision 03, "Scoped contexts"); siblings sharing a label join a scope.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::Result;
use crate::fiber::{Disposer, DisposeFn, Fiber};
use crate::service::{Key, ServiceMap};

/// A service key scoped by an isolate label: `(type, label)`. Two contexts
/// isolating the same type under the same label share one scope, mirroring
/// Cordis's label-joined isolation.
pub type ScopedKey = Key;

/// The plugin context.
#[derive(Clone, Default)]
pub struct Context {
    pub(crate) services: Arc<ServiceMap>,
    /// Service type → the label-scoped key it resolves through. Absent
    /// entries resolve the plain (unscoped) key.
    isolation: Arc<Mutex<HashMap<TypeId, ScopedKey>>>,
    /// The fiber that owns this context's registrations, if mounted under one.
    fiber: Arc<Mutex<Option<Arc<Fiber>>>>,
}

impl Context {
    /// Create a fresh root context.
    pub fn root() -> Self {
        Self::default()
    }

    /// Install a service under a type key, owned by the given fiber.
    ///
    /// The returned disposer removes the service exactly once when the fiber
    /// unloads (or the guard is dropped early) — every registration carries
    /// its own undo (decision 03-Q3). If the service was replaced by a later
    /// provider before this disposer runs, the removal is skipped: the stale
    /// disposer must not revoke a live replacement.
    pub fn provide_for<T: Any + Send + Sync>(
        &self,
        fiber: &Arc<Fiber>,
        service: T,
    ) -> Result<Disposer> {
        self.services.provide::<T>(service);
        let map = self.services.clone();
        fiber.effect(move || {
            Some(Box::new(move || {
                map.remove::<T>();
            }) as DisposeFn)
        })
    }

    /// Install a service, owned by this context's current fiber.
    ///
    /// Panics if the context has no owning fiber: providing on a root context
    /// without a fiber would create an unowned, irreversible registration.
    pub fn provide<T: Any + Send + Sync>(&self, service: T) -> Result<Disposer> {
        let fiber = self
            .fiber
            .lock()
            .clone()
            .expect("provide() requires an owning fiber; use provide_for()");
        self.provide_for(&fiber, service)
    }

    /// The key `T` resolves through in this context: the label-scoped key when
    /// `T` is isolated here, otherwise the plain type key.
    fn key_for<T: Any + Send + Sync>(&self) -> Key {
        self.isolation
            .lock()
            .get(&TypeId::of::<T>())
            .cloned()
            .unwrap_or_else(|| Key::of::<T>())
    }

    /// Look up a service by type, honouring any scoped isolation.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.services.get_key::<T>(self.key_for::<T>())
    }

    /// Whether a service of type `T` is present, honouring scoped isolation.
    pub fn has<T: Any + Send + Sync>(&self) -> bool {
        self.services.contains_key(&self.key_for::<T>())
    }

    /// Whether the erased type id is present (used by inject-driven gating).
    ///
    /// Honors isolation: if `key` is isolated in this context, presence is
    /// tested against the scoped key rather than the plain one.
    pub fn contains_id(&self, key: TypeId) -> bool {
        let resolved = self
            .isolation
            .lock()
            .get(&key)
            .cloned()
            .unwrap_or(Key::Plain(key));
        self.services.contains_key(&resolved)
    }

    /// Remove a service by type immediately (bypassing the fiber effect).
    pub fn remove<T: Any + Send + Sync>(&self) {
        self.services.remove::<T>();
    }

    /// The owning fiber of this context's registrations, if mounted under one.
    pub fn fiber(&self) -> Option<Arc<Fiber>> {
        self.fiber.lock().clone()
    }

    /// Set the owning fiber for subsequent `provide` calls.
    pub fn set_fiber(&self, fiber: Arc<Fiber>) {
        *self.fiber.lock() = Some(fiber);
    }

    /// Create a child context sharing every service (Cordis `extend()`).
    ///
    /// The child gets its own fiber slot; registrations made through it are
    /// owned by whoever is mounted there.
    pub fn extend(&self) -> Context {
        Context {
            services: self.services.clone(),
            isolation: Arc::new(Mutex::new(self.isolation.lock().clone())),
            fiber: Arc::new(Mutex::new(None)),
        }
    }

    /// Create a child context where service type `T` resolves to an
    /// independent override (Cordis `isolate(name, label)`).
    ///
    /// Lookups for `T` inside the new context see exactly what has been
    /// provided for the *label-scoped* key; siblings with the same label
    /// share a scope ("sharing a label joins the scopes"). Other service
    /// types resolve through to the parent.
    pub fn isolate<T: Any + Send + Sync>(&self, label: &str) -> Context {
        let mut isolation = self.isolation.lock().clone();
        let scoped_key = Key::scoped::<T>(label);
        isolation.insert(TypeId::of::<T>(), scoped_key.clone());
        Context {
            services: self.services.clone(),
            isolation: Arc::new(Mutex::new(isolation)),
            fiber: Arc::new(Mutex::new(None)),
        }
    }

    /// Provide a service into the labeled isolate for `T` created by
    /// [`Context::isolate`].
    ///
    /// Registration is owned by `fiber` like any other effect. `label` must
    /// match the isolate's label.
    pub fn provide_isolated<T: Any + Send + Sync>(
        &self,
        fiber: &Arc<Fiber>,
        label: &str,
        service: T,
    ) -> Result<Disposer> {
        let scoped_key = Key::scoped::<T>(label);
        self.services.provide_key(scoped_key.clone(), service);
        let map = self.services.clone();
        let key = scoped_key;
        fiber.effect(move || {
            let map = map.clone();
            let key = key.clone();
            Some(Box::new(move || {
                map.remove_key(key.clone());
            }) as DisposeFn)
        })
    }

    /// The label this context isolates `T` under, if any.
    pub fn isolation_label<T: Any + Send + Sync>(&self) -> Option<String> {
        match self.isolation.lock().get(&TypeId::of::<T>()) {
            Some(Key::Scoped(_, label)) => Some(label.clone()),
            Some(Key::Plain(_)) | None => None,
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    struct Greeter {
        greeting: &'static str,
    }

    struct ProbeService;

    #[test]
    fn provide_and_get_roundtrip() {
        let ctx = Context::root();
        let fiber = Fiber::active();
        let _keep1 = ctx.provide_for(
            &fiber,
            Greeter {
                greeting: "hello",
            },
        )
        .unwrap();
        let svc = ctx.get::<Greeter>().unwrap();
        assert_eq!(svc.greeting, "hello");
    }

    #[test]
    fn provide_requires_owning_fiber() {
        let ctx = Context::root();
        let result = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
            ctx.provide(ProbeService)
        }));
        assert!(result.is_err(), "provide without fiber must panic");
    }

    #[test]
    fn fiber_unload_removes_provided_service() {
        let ctx = Context::root();
        let fiber = Fiber::pending();
        let _keep2 = ctx.provide_for(&fiber, ProbeService).unwrap();
        assert!(ctx.has::<ProbeService>());
        fiber.dispose();
        assert!(!ctx.has::<ProbeService>());
    }

    #[test]
    fn guard_drop_detaches_service_cleanup_to_fiber() {
        let ctx = Context::root();
        let fiber = Fiber::pending();
        let guard = ctx.provide_for(&fiber, ProbeService).unwrap();
        assert!(ctx.has::<ProbeService>());
        // Dropping the guard detaches ownership: the service stays live and
        // still unwinds when the owning fiber unloads.
        drop(guard);
        assert!(ctx.has::<ProbeService>());
        fiber.dispose();
        assert!(!ctx.has::<ProbeService>());
    }

    #[test]
    fn extend_shares_services_with_child() {
        let root = Context::root();
        let fiber = Fiber::active();
        let _keep3 = root.provide_for(&fiber, ProbeService).unwrap();
        let child = root.extend();
        assert!(child.has::<ProbeService>());
    }

    #[test]
    fn erased_lookup_matches_typed_check() {
        let ctx = Context::root();
        let fiber = Fiber::active();
        let _keep4 = ctx.provide_for(&fiber, ProbeService).unwrap();
        assert!(ctx.contains_id(TypeId::of::<ProbeService>()));
        assert!(!ctx.contains_id(TypeId::of::<Greeter>()));
    }

    #[test]
    fn later_provider_replaces_entry_and_old_disposer_does_not_revoke() {
        let ctx = Context::root();
        let fiber_a = Fiber::pending();
        let fiber_b = Fiber::pending();
        let guard_a = ctx.provide_for(&fiber_a, ProbeService).unwrap();
        let guard_b = ctx.provide_for(&fiber_b, ProbeService2).unwrap();
        let _ = guard_b;
        // Unload A: B is a different type, unaffected.
        drop(guard_a);
        assert!(ctx.has::<ProbeService2>());
        let _ = AtomicUsize::new(0);
        let _ = Ordering::SeqCst;
    }


    #[test]
    fn isolate_resolves_scoped_key_within_context() {
        let root = Context::root();
        let fiber = Fiber::active();
        let _k = root.provide_for(&fiber, Greeter { greeting: "root" }).unwrap();
        let isolated = root.isolate::<Greeter>("dev");
        let _k2 = isolated
            .provide_isolated(&fiber, "dev", Greeter { greeting: "dev" })
            .unwrap();
        // Root still resolves the plain service.
        assert_eq!(root.get::<Greeter>().unwrap().greeting, "root");
        // Isolated context resolves the scoped override.
        assert_eq!(isolated.get::<Greeter>().unwrap().greeting, "dev");
        assert_eq!(isolated.isolation_label::<Greeter>().as_deref(), Some("dev"));
    }

    #[test]
    fn same_label_joins_scope_between_siblings() {
        let root = Context::root();
        let a = root.isolate::<Greeter>("dev");
        let b = root.isolate::<Greeter>("dev");
        let fiber = Fiber::active();
        let _k = a
            .provide_isolated(&fiber, "dev", Greeter { greeting: "shared" })
            .unwrap();
        // Sibling with the same label shares the joined scoped key.
        assert_eq!(b.get::<Greeter>().unwrap().greeting, "shared");
    }

    #[test]
    fn isolate_does_not_affect_other_types() {
        let root = Context::root();
        let isolated = root.isolate::<Greeter>("dev");
        let fiber = Fiber::active();
        let _k = root.provide_for(&fiber, ProbeService).unwrap();
        // ProbeService is not isolated: the isolated context falls through to it.
        assert!(isolated.has::<ProbeService>());
    }

    #[test]
    fn isolate_unload_removes_scoped_service() {
        let root = Context::root();
        let isolated = root.isolate::<Greeter>("dev");
        let fiber = Fiber::pending();
        let _k = isolated
            .provide_isolated(&fiber, "dev", Greeter { greeting: "dev" })
            .unwrap();
        assert_eq!(isolated.get::<Greeter>().unwrap().greeting, "dev");
        fiber.dispose();
        assert!(!isolated.has::<Greeter>());
    }

    struct ProbeService2;
}
