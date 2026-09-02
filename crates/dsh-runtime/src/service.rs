//! The type-token service map.
//!
//! This is the core of the "everything is a plugin" architecture: services are
//! stored as `Arc<dyn Any + Send + Sync>` keyed by their Rust type. The type is
//! the stable spelling of dsh's `ctx.<key>` string lookup — consumers name the
//! capability, never the implementation, and this holds as a static fact.
//!
//! The raw map is internal to the runtime; consumers reach services through
//! typed accessors (`Context::get::<S>()`) that downcast exactly once at the
//! typed boundary, so a downcast failure is unreachable at typed call sites.

use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// Stable identifier for a service, expressed as its Rust type identity.
///
/// Mirrors `ServiceId` from the design: the Rust type *is* the stable key.
pub type ServiceId = TypeId;

/// Resolve the [`ServiceId`] of a service type `T`.
pub fn service_id<T: ?Sized + 'static>() -> ServiceId {
    record_service_name::<T>();
    TypeId::of::<T>()
}

/// Best-effort Rust type name for diagnostics.
pub fn service_name(id: ServiceId) -> &'static str {
    NAME_TABLE.read().get(&id).copied().unwrap_or("<unknown>")
}

/// A boxed service value. Services must be `Send + Sync` because the whole
/// service layer runs on a multi-thread tokio runtime (decision 03-Q5).
pub type BoxedService = Arc<dyn Any + Send + Sync>;

/// The type-token keyed service map.
///
/// Reads are concurrent (the map is read-heavy); registration and removal are
/// exclusive. Registration is an *effect* owned by the fiber that made it, so
/// unloading a fiber removes exactly the services it installed — see
/// [`crate::context::Context::provide`].
#[derive(Default)]
pub struct ServiceMap {
    inner: RwLock<HashMap<TypeId, BoxedService>>,
}

impl ServiceMap {
    /// Create an empty service map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install an already type-erased service under an explicit key.
    pub(crate) fn provide_boxed(
        &self,
        id: ServiceId,
        service: BoxedService,
    ) -> std::result::Result<(), BoxedService> {
        use std::collections::hash_map::Entry;

        match self.inner.write().entry(id) {
            Entry::Vacant(entry) => {
                entry.insert(service);
                Ok(())
            }
            Entry::Occupied(_) => Err(service),
        }
    }

    /// Remove `id` only when it still points at `expected`.
    pub(crate) fn remove_if_same(&self, id: ServiceId, expected: &BoxedService) -> bool {
        let mut services = self.inner.write();
        let is_same = services
            .get(&id)
            .is_some_and(|current| Arc::ptr_eq(current, expected));
        if is_same {
            services.remove(&id);
        }
        is_same
    }

    pub(crate) fn get_boxed(&self, id: ServiceId) -> Option<BoxedService> {
        self.inner.read().get(&id).cloned()
    }
}

/// Best-effort `TypeId -> name` side table for diagnostics.
static NAME_TABLE: std::sync::LazyLock<RwLock<HashMap<TypeId, &'static str>>> =
    std::sync::LazyLock::new(|| RwLock::new(HashMap::new()));

/// Record the `&'static str` name of a service type for diagnostics.
fn record_service_name<T: ?Sized + 'static>() -> &'static str {
    let name: &'static str = type_name::<T>();
    NAME_TABLE.write().insert(TypeId::of::<T>(), name);
    name
}
