//! Scoped service contexts.

use std::any::{Any, TypeId};
use std::sync::Arc;

use crate::error::Result;
use crate::fiber::{DisposeFn, Disposer, Fiber};
use crate::service::{service_id, BoxedService, ServiceMap};

struct ContextInner {
    parent: Option<Context>,
    services: Arc<ServiceMap>,
    fiber: Fiber,
}

/// A scoped repository of services and the fiber owning its registrations.
#[derive(Clone)]
pub struct Context(Arc<ContextInner>);

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// Create an empty root context.
    pub fn new() -> Self {
        Self::with_fiber(Fiber::default())
    }

    /// Create an empty root context owned by `fiber`.
    pub fn with_fiber(fiber: Fiber) -> Self {
        Self(Arc::new(ContextInner {
            parent: None,
            services: Arc::new(ServiceMap::new()),
            fiber,
        }))
    }

    /// Create a child scope that inherits services and may shadow them locally.
    pub fn extend(&self) -> Self {
        Self(Arc::new(ContextInner {
            parent: Some(self.clone()),
            services: Arc::new(ServiceMap::new()),
            fiber: Fiber::default(),
        }))
    }

    /// The fiber that owns registrations made through this context.
    pub fn fiber(&self) -> &Fiber {
        &self.0.fiber
    }

    /// Use this same service scope with a different registration owner.
    pub(crate) fn owned_by(&self, fiber: Fiber) -> Self {
        Self(Arc::new(ContextInner {
            parent: self.0.parent.clone(),
            services: self.0.services.clone(),
            fiber,
        }))
    }

    pub(crate) fn contains_id(&self, id: TypeId) -> bool {
        self.get_boxed(id).is_some()
    }

    /// Resolve a service locally, then through ancestor scopes.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        let boxed = self.get_boxed(TypeId::of::<T>())?;
        boxed.downcast::<T>().ok()
    }

    fn get_boxed(&self, id: TypeId) -> Option<BoxedService> {
        self.0
            .services
            .get_boxed(id)
            .or_else(|| self.0.parent.as_ref()?.get_boxed(id))
    }

    /// Provide a service in this scope until the returned disposer is dropped.
    pub fn provide<T: Any + Send + Sync>(&self, service: T) -> Result<Disposer> {
        self.provide_arc(Arc::new(service))
    }

    /// Provide an `Arc` service in this scope until the disposer is dropped.
    pub fn provide_arc<T: Any + Send + Sync>(&self, service: Arc<T>) -> Result<Disposer> {
        self.0.fiber.assert_active()?;
        let id = service_id::<T>();
        let boxed: BoxedService = service;
        self.0
            .services
            .provide_boxed(id, boxed.clone())
            .map_err(|_| crate::error::RuntimeError::duplicate_service())?;
        let context = self.clone();
        self.0.fiber.effect(move || {
            let mut registration = Some((context, boxed));
            Some(Box::new(move || {
                if let Some((context, boxed)) = registration.take() {
                    context.0.services.remove_if_same(id, &boxed);
                }
            }) as DisposeFn)
        })
    }

    /// Register any reversible effect on this context's fiber.
    pub fn effect<F>(&self, body: F) -> Result<Disposer>
    where
        F: FnOnce() -> Option<DisposeFn> + Send + 'static,
    {
        self.0.fiber.effect(body)
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn child_scope_inherits_and_can_shadow_a_service() {
        let root = Context::new();
        let _root_service = root.provide(String::from("root")).unwrap();
        let child = root.extend();
        assert_eq!(
            child.get::<String>().as_deref().map(String::as_str),
            Some("root")
        );

        let child_service = child.provide(String::from("child")).unwrap();
        assert_eq!(
            child.get::<String>().as_deref().map(String::as_str),
            Some("child")
        );
        drop(child_service);
        assert_eq!(
            child.get::<String>().as_deref().map(String::as_str),
            Some("root")
        );
    }

    #[test]
    fn one_scope_refuses_two_providers_for_the_same_service() {
        let context = Context::new();
        let _provider = context.provide(1_u32).unwrap();
        let error = match context.provide(2_u32) {
            Ok(_) => panic!("duplicate provider was accepted"),
            Err(error) => error,
        };
        assert_eq!(error.code, "DUPLICATE_SERVICE");
        assert_eq!(context.get::<u32>().as_deref(), Some(&1));
    }

    #[test]
    fn disposing_the_fiber_removes_its_services() {
        let context = Context::new();
        let _service = context.provide(7_u32).unwrap();
        context.fiber().dispose();
        assert!(context.get::<u32>().is_none());
    }
}
