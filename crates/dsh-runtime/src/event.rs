//! Typed event dispatch with Cordis-compatible modes.

use std::future::Future;
use std::pin::Pin;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use futures::future::join_all;
use futures::FutureExt;
use parking_lot::Mutex;

use crate::error::{Result, RuntimeError};
use crate::fiber::{DisposeFn, Disposer, Fiber};

/// A boxed, `Send` future borrowing its arguments.
pub type BoxFuture<'a, T> = Pin<Box<dyn Future<Output = T> + Send + 'a>>;

type EmitFn<E> = dyn Fn(&E) -> Result<()> + Send + Sync;
type BailFn<E, R> = dyn Fn(&E) -> Result<Option<R>> + Send + Sync;
type AsyncFn<E, R> = dyn for<'a> Fn(&'a E) -> BoxFuture<'a, Result<Option<R>>> + Send + Sync;
type WaterfallFn<E, R> =
    dyn for<'a> Fn(&'a mut E, Next<'a, E, R>) -> BoxFuture<'a, Result<Option<R>>> + Send + Sync;
type TerminalFn<E, R> = dyn for<'a> Fn(&'a mut E) -> BoxFuture<'a, Result<Option<R>>> + Send + Sync;

struct Slot<T: ?Sized> {
    active: Arc<AtomicBool>,
    listener: Arc<T>,
}

impl<T: ?Sized> Clone for Slot<T> {
    fn clone(&self) -> Self {
        Self {
            active: self.active.clone(),
            listener: self.listener.clone(),
        }
    }
}

/// A registration that is removed when dropped.
pub struct Subscription {
    active: Arc<AtomicBool>,
}

/// Where a listener is inserted in its dispatch chain.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum ListenerOrder {
    Append,
    Prepend,
}

impl Subscription {
    /// Transfer this listener registration to a plugin fiber.
    pub fn bind(self, fiber: &Fiber) -> Result<Disposer> {
        let mut subscription = Some(self);
        fiber.effect(move || {
            Some(Box::new(move || {
                subscription.take();
            }) as DisposeFn)
        })
    }
}

impl Drop for Subscription {
    fn drop(&mut self) {
        self.active.store(false, Ordering::Release);
    }
}

/// One typed event domain supporting all five dispatch modes.
pub struct EventRegistry<E, R> {
    emit: Mutex<Vec<Slot<EmitFn<E>>>>,
    bail: Mutex<Vec<Slot<BailFn<E, R>>>>,
    asynchronous: Mutex<Vec<Slot<AsyncFn<E, R>>>>,
    waterfall: Mutex<Vec<Slot<WaterfallFn<E, R>>>>,
}

impl<E, R> Default for EventRegistry<E, R> {
    fn default() -> Self {
        Self {
            emit: Mutex::new(Vec::new()),
            bail: Mutex::new(Vec::new()),
            asynchronous: Mutex::new(Vec::new()),
            waterfall: Mutex::new(Vec::new()),
        }
    }
}

impl<E, R> EventRegistry<E, R>
where
    E: Send + Sync,
    R: Send,
{
    pub fn new() -> Self {
        Self::default()
    }

    /// Register a synchronous observer at the requested position.
    pub fn on_emit<F>(&self, order: ListenerOrder, listener: F) -> Subscription
    where
        F: Fn(&E) -> Result<()> + Send + Sync + 'static,
    {
        insert(&self.emit, order, Arc::new(listener))
    }

    /// Observe synchronously in registration order; isolate and return failures.
    pub fn emit(&self, event: &E) -> Vec<RuntimeError> {
        let errors: Vec<_> = snapshot(&self.emit)
            .into_iter()
            .filter_map(|listener| {
                std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| listener(event)))
                    .unwrap_or_else(|_| Err(RuntimeError::listener_panicked()))
                    .err()
            })
            .collect();
        log_failures(&errors);
        errors
    }

    pub fn on_bail<F>(&self, order: ListenerOrder, listener: F) -> Subscription
    where
        F: Fn(&E) -> Result<Option<R>> + Send + Sync + 'static,
    {
        insert(&self.bail, order, Arc::new(listener))
    }

    /// Run synchronous listeners until one returns a value.
    pub fn bail(&self, event: &E) -> Result<Option<R>> {
        for listener in snapshot(&self.bail) {
            if let Some(result) = listener(event)? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    pub fn on_async<F>(&self, order: ListenerOrder, listener: F) -> Subscription
    where
        F: for<'a> Fn(&'a E) -> BoxFuture<'a, Result<Option<R>>> + Send + Sync + 'static,
    {
        insert(&self.asynchronous, order, Arc::new(listener))
    }

    /// Await every listener concurrently and contain each outcome.
    pub async fn parallel(&self, event: &E) -> Vec<Result<Option<R>>> {
        let outcomes = join_all(snapshot(&self.asynchronous).into_iter().map(
            |listener| async move {
                std::panic::AssertUnwindSafe(listener(event))
                    .catch_unwind()
                    .await
                    .unwrap_or_else(|_| Err(RuntimeError::listener_panicked()))
            },
        ))
        .await;
        let errors: Vec<_> = outcomes
            .iter()
            .filter_map(|outcome| outcome.as_ref().err().cloned())
            .collect();
        log_failures(&errors);
        outcomes
    }

    /// Await listeners in registration order until one returns a value.
    pub async fn serial(&self, event: &E) -> Result<Option<R>> {
        for listener in snapshot(&self.asynchronous) {
            if let Some(result) = listener(event).await? {
                return Ok(Some(result));
            }
        }
        Ok(None)
    }

    pub fn on_waterfall<F>(&self, order: ListenerOrder, listener: F) -> Subscription
    where
        F: for<'a> Fn(&'a mut E, Next<'a, E, R>) -> BoxFuture<'a, Result<Option<R>>>
            + Send
            + Sync
            + 'static,
    {
        insert(&self.waterfall, order, Arc::new(listener))
    }

    /// Dispatch an around-middleware chain. A listener vetoes by not calling `next`.
    pub async fn waterfall<F>(&self, event: &mut E, terminal: F) -> Result<Option<R>>
    where
        F: for<'a> Fn(&'a mut E) -> BoxFuture<'a, Result<Option<R>>> + Send + Sync + 'static,
    {
        self.dispatch_from(0, event, &terminal).await
    }

    fn dispatch_from<'a>(
        &'a self,
        index: usize,
        event: &'a mut E,
        terminal: &'a TerminalFn<E, R>,
    ) -> BoxFuture<'a, Result<Option<R>>> {
        Box::pin(async move {
            let listener = snapshot(&self.waterfall).into_iter().nth(index);
            if let Some(listener) = listener {
                listener(
                    event,
                    Next {
                        registry: self,
                        index: index + 1,
                        terminal,
                    },
                )
                .await
            } else {
                terminal(event).await
            }
        })
    }
}

/// The single-use conceptual continuation passed to waterfall listeners.
pub struct Next<'a, E, R> {
    registry: &'a EventRegistry<E, R>,
    index: usize,
    terminal: &'a TerminalFn<E, R>,
}

impl<'a, E, R> Next<'a, E, R>
where
    E: Send + Sync,
    R: Send,
{
    pub fn run(self, event: &'a mut E) -> BoxFuture<'a, Result<Option<R>>> {
        self.registry
            .dispatch_from(self.index, event, self.terminal)
    }
}

fn insert<T: ?Sized>(
    slots: &Mutex<Vec<Slot<T>>>,
    order: ListenerOrder,
    listener: Arc<T>,
) -> Subscription {
    let active = Arc::new(AtomicBool::new(true));
    let slot = Slot {
        active: active.clone(),
        listener,
    };
    let mut slots = slots.lock();
    if order == ListenerOrder::Prepend {
        slots.insert(0, slot);
    } else {
        slots.push(slot);
    }
    Subscription { active }
}

fn log_failures(errors: &[RuntimeError]) {
    for error in errors {
        tracing::error!(
            code = error.code,
            message = error.message,
            "event listener failed"
        );
    }
}

fn snapshot<T: ?Sized>(slots: &Mutex<Vec<Slot<T>>>) -> Vec<Arc<T>> {
    let mut slots = slots.lock();
    slots.retain(|slot| slot.active.load(Ordering::Acquire));
    slots.iter().map(|slot| slot.listener.clone()).collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Mutex as StdMutex;

    #[test]
    fn emit_observes_prepend_order_and_contains_failures() {
        let registry = EventRegistry::<(), ()>::new();
        let order = Arc::new(StdMutex::new(Vec::new()));
        let a = order.clone();
        let _a = registry.on_emit(ListenerOrder::Append, move |_| {
            a.lock().unwrap().push("ordinary");
            Err(RuntimeError::new("OBSERVER", "failed"))
        });
        let b = order.clone();
        let _b = registry.on_emit(ListenerOrder::Prepend, move |_| {
            b.lock().unwrap().push("prepended");
            Ok(())
        });
        let errors = registry.emit(&());
        assert_eq!(*order.lock().unwrap(), ["prepended", "ordinary"]);
        assert_eq!(errors.len(), 1);
    }

    #[test]
    fn emit_contains_panics_and_continues_to_siblings() {
        let registry = EventRegistry::<(), ()>::new();
        let _panic = registry.on_emit(ListenerOrder::Append, |_| panic!("observer panic"));
        let reached = Arc::new(AtomicBool::new(false));
        let marker = reached.clone();
        let _sibling = registry.on_emit(ListenerOrder::Append, move |_| {
            marker.store(true, Ordering::SeqCst);
            Ok(())
        });

        let errors = registry.emit(&());
        assert_eq!(errors[0].code, "LISTENER_PANICKED");
        assert!(reached.load(Ordering::SeqCst));
    }

    #[test]
    fn bail_stops_on_first_synchronous_value() {
        let registry = EventRegistry::<(), u8>::new();
        let _a = registry.on_bail(ListenerOrder::Append, |_| Ok(None));
        let _b = registry.on_bail(ListenerOrder::Append, |_| Ok(Some(2)));
        let _c = registry.on_bail(ListenerOrder::Append, |_| Ok(Some(3)));
        assert_eq!(registry.bail(&()).unwrap(), Some(2));
    }

    #[tokio::test]
    async fn parallel_waits_for_all_and_contains_failures() {
        let registry = EventRegistry::<(), u8>::new();
        let _a = registry.on_async(ListenerOrder::Append, |_| Box::pin(async { Ok(Some(1)) }));
        let _b = registry.on_async(ListenerOrder::Append, |_| {
            Box::pin(async { Err(RuntimeError::new("LISTENER", "failed")) })
        });
        let outcomes = registry.parallel(&()).await;
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0], Ok(Some(1)));
        assert!(outcomes[1].is_err());
    }

    #[tokio::test]
    async fn parallel_contains_a_panicking_future() {
        let registry = EventRegistry::<(), ()>::new();
        let _panic = registry.on_async(ListenerOrder::Append, |_| {
            Box::pin(async { panic!("async observer panic") })
        });
        let _sibling = registry.on_async(ListenerOrder::Append, |_| Box::pin(async { Ok(None) }));

        let outcomes = registry.parallel(&()).await;
        assert_eq!(outcomes.len(), 2);
        assert_eq!(outcomes[0].as_ref().unwrap_err().code, "LISTENER_PANICKED");
        assert!(outcomes[1].is_ok());
    }

    #[tokio::test]
    async fn serial_stops_on_first_async_value() {
        let registry = EventRegistry::<(), u8>::new();
        let _a = registry.on_async(ListenerOrder::Append, |_| Box::pin(async { Ok(None) }));
        let _b = registry.on_async(ListenerOrder::Append, |_| Box::pin(async { Ok(Some(2)) }));
        let _c = registry.on_async(ListenerOrder::Append, |_| Box::pin(async { Ok(Some(3)) }));
        assert_eq!(registry.serial(&()).await.unwrap(), Some(2));
    }

    #[tokio::test]
    async fn waterfall_wraps_terminal_and_can_veto() {
        let registry = EventRegistry::<Vec<&'static str>, usize>::new();
        let _outer = registry.on_waterfall(ListenerOrder::Append, |event, next| {
            Box::pin(async move {
                event.push("before");
                let result = next.run(event).await?;
                event.push("after");
                Ok(result.map(|value| value + 1))
            })
        });
        let mut event = Vec::new();
        let result = registry
            .waterfall(&mut event, |event| {
                Box::pin(async move {
                    event.push("terminal");
                    Ok(Some(1))
                })
            })
            .await
            .unwrap();
        assert_eq!(event, ["before", "terminal", "after"]);
        assert_eq!(result, Some(2));

        let vetoes = EventRegistry::<(), usize>::new();
        let _veto = vetoes.on_waterfall(ListenerOrder::Append, |_, _| Box::pin(async { Ok(None) }));
        let called = Arc::new(AtomicBool::new(false));
        let marker = called.clone();
        assert_eq!(
            vetoes
                .waterfall(&mut (), move |_| {
                    let marker = marker.clone();
                    Box::pin(async move {
                        marker.store(true, Ordering::SeqCst);
                        Ok(Some(1))
                    })
                })
                .await
                .unwrap(),
            None
        );
        assert!(!called.load(Ordering::SeqCst));
    }
}
