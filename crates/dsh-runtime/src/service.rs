//! The type-token service map.
//!
//! This is the core of the "everything is a plugin" architecture: services are
//! stored as `Arc<dyn Any + Send + Sync>` keyed by their Rust type. The type is
//! the stable spelling of dsh's `ctx.<key>` string lookup — consumers name the
//! capability, never the implementation, and this holds as a static fact.
//!
//! The raw map is internal to the runtime; consumers reach services through
//! typed accessors (`Context::get::<S>()`) that downcast exactly once at the
//! typed boundary.

use std::any::{type_name, Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::RwLock;

/// Stable identifier for a service, expressed as its Rust type identity.
pub type ServiceId = TypeId;

/// The storage key for a service.
///
/// A service may be installed at its plain type identity, or under a
/// label-scoped key (Cordis `isolate`). Scoped keys let sibling contexts share
/// one override under a common label while leaving other types to resolve
/// through to the parent.
#[derive(Clone, PartialEq, Eq, Hash)]
pub enum Key {
    Plain(TypeId),
    Scoped(TypeId, String),
}

impl Key {
    /// The plain type-identity key for `T`.
    pub fn of<T: Any + Send + Sync>() -> Self {
        Key::Plain(TypeId::of::<T>())
    }

    /// The label-scoped key for `T` under `label`.
    pub fn scoped<T: Any + Send + Sync>(label: impl Into<String>) -> Self {
        Key::Scoped(TypeId::of::<T>(), label.into())
    }

    /// The unerased type identity of the service this key names.
    pub fn type_id(&self) -> TypeId {
        match self {
            Key::Plain(id) | Key::Scoped(id, _) => *id,
        }
    }
}

impl From<TypeId> for Key {
    fn from(id: TypeId) -> Self {
        Key::Plain(id)
    }
}

/// A boxed service value. Services must be `Send + Sync` because the whole
/// service layer runs on a multi-thread tokio runtime (decision 03-Q5).
pub type BoxedService = Arc<dyn Any + Send + Sync>;

#[derive(Default)]
pub struct ServiceMap {
    inner: RwLock<HashMap<Key, BoxedService>>,
    names: RwLock<HashMap<Key, &'static str>>,
}

impl ServiceMap {
    /// Create an empty service map.
    pub fn new() -> Self {
        Self::default()
    }

    /// Install a service at its plain type key, replacing any prior value and
    /// returning the displaced one.
    pub fn provide<T: Any + Send + Sync>(&self, service: T) -> Option<BoxedService> {
        self.provide_key(Key::of::<T>(), service)
    }

    /// Install a service under an explicit (possibly label-scoped) key.
    pub fn provide_key<T: Any + Send + Sync>(
        &self,
        key: Key,
        service: T,
    ) -> Option<BoxedService> {
        self.names.write().insert(key.clone(), type_name::<T>());
        self.inner.write().insert(key, Arc::new(service))
    }

    /// Look up a service by type, returning an `Arc` to it.
    ///
    /// The returned handle outlives any concurrent re-registration.
    pub fn get<T: Any + Send + Sync>(&self) -> Option<Arc<T>> {
        self.get_key(Key::of::<T>())
    }

    /// Look up a service under an explicit (possibly label-scoped) key.
    pub fn get_key<T: Any + Send + Sync>(&self, key: Key) -> Option<Arc<T>> {
        let boxed = self.inner.read().get(&key)?.clone();
        boxed.downcast::<T>().ok()
    }

    /// Remove and return the service of type `T`, if present.
    pub fn remove<T: Any + Send + Sync>(&self) -> Option<BoxedService> {
        self.remove_key(Key::of::<T>())
    }

    /// Remove and return the service under an explicit key.
    pub fn remove_key(&self, key: Key) -> Option<BoxedService> {
        self.names.write().remove(&key);
        self.inner.write().remove(&key)
    }

    /// Whether a service of type `T` is currently present at its plain key.
    pub fn contains<T: Any + Send + Sync>(&self) -> bool {
        self.inner.read().contains_key(&Key::of::<T>())
    }

    /// Whether a service is present under an explicit (possibly scoped) key.
    pub fn contains_key(&self, key: &Key) -> bool {
        self.inner.read().contains_key(key)
    }

    /// Whether a service with the given plain type identity is present
    /// (erased lookup, as used by plugin inject gates).
    pub fn contains_id(&self, id: TypeId) -> bool {
        self.inner.read().contains_key(&Key::Plain(id))
    }

    /// Number of installed services.
    pub fn len(&self) -> usize {
        self.inner.read().len()
    }
    pub fn is_empty(&self) -> bool {
        self.len() == 0
    }

    /// Diagnostic name for the service type at `id`.
    pub fn name_of(&self, id: TypeId) -> &'static str {
        self.names
            .read()
            .get(&Key::Plain(id))
            .copied()
            .unwrap_or("<unknown>")
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    struct Marker;
    struct Other;

    #[test]
    fn provide_and_get_by_type() {
        let map = ServiceMap::new();
        map.provide::<Marker>(Marker);
        assert!(map.get::<Marker>().is_some());
        assert!(map.get::<Other>().is_none());
    }

    #[test]
    fn replace_returns_prior_value() {
        let map = ServiceMap::new();
        assert!(map.provide::<Marker>(Marker).is_none());
        assert!(map.provide::<Marker>(Marker).is_some());
    }

    #[test]
    fn remove_clears_entry() {
        let map = ServiceMap::new();
        map.provide::<Marker>(Marker);
        map.remove::<Marker>();
        assert!(!map.contains::<Marker>());
        assert!(map.get::<Marker>().is_none());
    }

    #[test]
    fn erased_contains_id() {
        let map = ServiceMap::new();
        map.provide::<Marker>(Marker);
        assert!(map.contains_id(TypeId::of::<Marker>()));
        assert!(!map.contains_id(TypeId::of::<Other>()));
    }

    #[test]
    fn records_type_names_for_diagnostics() {
        let map = ServiceMap::new();
        map.provide::<Marker>(Marker);
        assert!(map.name_of(TypeId::of::<Marker>()).contains("Marker"));
    }
}
