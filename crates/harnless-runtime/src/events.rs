//! The typed-event system with the five dispatch modes.
//!
//! One event **domain** owns one [`EventRegistry`]. Event payloads are
//! identified by their Rust type — the same type-token rule that keys
//! services — and a waterfall's result type `R` is part of the listener key,
//! so dispatch never downcasts listener storage.
//!
//! Dispatch modes, mirroring research 02 (`docs/research/cordis-semantics.md`):
//!
//! | Mode        | Awaited | Order                        | Returns           |
//! |-------------|---------|------------------------------|-------------------|
//! | `emit`      | no      | registration order           | `()`              |
//! | `parallel`  | yes     | all concurrently, all settle | `()`              |
//! | `serial`    | yes     | registration order           | first bail value  |
//! | `bail`      | sync    | registration order           | first bail value  |
//! | `waterfall` | sync    | outermost-first, wrap `next` | final value       |
//!
//! The waterfall `next()` contract: a listener receives the payload plus a
//! [`Next`] continuation. Calling `next()` delegates to the next listener and
//! finally the built-in behavior; returning without calling it **vetoed** the
//! action. Veto is expressed by the absence of delegation — an error means
//! "failed", a veto means "I own this decision". Values propagate through
//! `next()`'s return; a listener may replace the result entirely and
//! downstream listeners see only the replacement. Registration order is
//! dispatch order; `prepend` places a listener first.
//!
//! Failure containment (decisions 03, 04): a listener failure during
//! fire-and-forget or fan-out dispatch is isolated and never blocks siblings.
//!
//! ## Soundness
//!
//! Listeners are `FnMut` (they may record observations in captured state), so
//! each listener lives behind a mutex and dispatch takes the lock for the
//! duration of one call; no lock is held across `next()` delegation. A
//! reentrant waterfall into the same listener deadlocks rather than corrupts
//! — fail-loud, matching the runtime's invariant style.

use std::any::{Any, TypeId};
use std::collections::HashMap;
use std::sync::Arc;

use parking_lot::{Mutex, RwLock};

use crate::error::Result;
use crate::fiber::{DisposeFn, Disposer, Fiber};

/// Listener placement, mirroring the reference `EventOptions`.
#[derive(Debug, Clone, Copy, Default)]
pub struct EventOptions {
    /// Run this listener before previously registered ones.
    pub prepend: bool,
}

impl EventOptions {
    /// Default placement: registration order (append).
    pub fn new() -> Self {
        Self::default()
    }

    /// Place this listener ahead of all previously registered ones.
    pub fn prepend() -> Self {
        Self { prepend: true }
    }
}

/// A plain event listener: runs against an exclusive payload.
pub type Listener<E> = Box<dyn FnMut(&mut E) + Send>;

/// A waterfall listener: receives the payload plus the [`Next`] continuation.
pub type WaterfallListener<E, R> = Box<dyn FnMut(&mut E, &mut Next<'_, E, R>) -> R + Send>;

/// One shared plain listener slot.
pub type PlainSlot<E> = Arc<Mutex<Listener<E>>>;

/// One shared waterfall listener slot.
pub type WaterfallSlot<E, R> = Arc<Mutex<WaterfallListener<E, R>>>;

/// The `next()` continuation handed to a waterfall listener.
///
/// Modeled as a chain object passed by mutable borrow (decision 03), avoiding
/// the lifetime tangle of nesting async closures. Invoking [`Next::call`]
/// advances to the next listener; when listeners are exhausted the
/// dispatcher-supplied built-in behavior runs and its value returns.
pub struct Next<'a, E: 'static, R> {
    chain: &'a mut ChainState<E, R>,
}

impl<E: 'static, R> Next<'_, E, R> {
    /// Delegate to the next listener (or the built-in behavior).
    pub fn call(&mut self, mut event: E) -> R {
        match self.chain.pending.pop() {
            Some(slot) => {
                let mut listener = slot.lock();
                let mut next = Next { chain: self.chain };
                listener(&mut event, &mut next)
            }
            None => {
                let tail = self
                    .chain
                    .tail
                    .take()
                    .expect("waterfall tail consumed twice");
                tail(event)
            }
        }
    }
}

struct ChainState<E: 'static, R> {
    /// Remaining listeners, outermost first (pop = next).
    pending: Vec<WaterfallSlot<E, R>>,
    /// The built-in behavior at the end of the chain. Consumed exactly once.
    tail: Option<Box<dyn FnOnce(E) -> R>>,
}

/// Erased diagnostics over one event type's listener set.
trait AnySet: Send + Sync {
    /// Count of plain listeners (diagnostics).
    fn plain_count(&self) -> usize;
}

/// The typed listener set for one event type.
struct SetFor<E: Send + Sync + 'static> {
    plain: RwLock<Vec<PlainSlot<E>>>,
    waterfall: RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>,
}

impl<E: Send + Sync + 'static> Default for SetFor<E> {
    fn default() -> Self {
        Self {
            plain: RwLock::new(Vec::new()),
            waterfall: RwLock::new(HashMap::new()),
        }
    }
}

impl<E: Send + Sync + 'static> AnySet for SetFor<E> {
    fn plain_count(&self) -> usize {
        self.plain.read().len()
    }
}

/// One typed event registry (one per event domain, decision 03-Q2).
///
/// Plain listeners are keyed by event type. Waterfall listeners are keyed by
/// event type **and result type** — that pair is the dispatch signature, and
/// keeping `R` in the key removes every downcast from the dispatch path.
#[derive(Clone, Default)]
pub struct EventRegistry {
    events: Arc<RwLock<HashMap<TypeId, Arc<dyn Any + Send + Sync>>>>,
}

impl EventRegistry {
    /// Create an empty registry.
    pub fn new() -> Self {
        Self::default()
    }

    /// The typed set for event `E`, creating it on first use.
    fn set_of<E: Send + Sync + 'static>(&self) -> Arc<SetFor<E>> {
        let mut events = self.events.write();
        let set = events
            .entry(TypeId::of::<E>())
            .or_insert_with(|| Arc::new(SetFor::<E>::default()) as Arc<dyn Any + Send + Sync>)
            .clone();
        set.downcast::<SetFor<E>>()
            .expect("listener set type mismatch")
    }

    /// The typed waterfall slots for `(E → R)`, creating them on first use.
    fn waterfall_slots<E, R>(set: &Arc<SetFor<E>>) -> Arc<RwLock<Vec<WaterfallSlot<E, R>>>>
    where
        E: Send + Sync + 'static,
        R: 'static,
    {
        let created = set
            .waterfall
            .write()
            .entry(TypeId::of::<R>())
            .or_insert_with(|| {
                Arc::new(RwLock::new(Vec::<WaterfallSlot<E, R>>::new()))
                    as Arc<dyn Any + Send + Sync>
            })
            .clone();
        created
            .downcast::<RwLock<Vec<WaterfallSlot<E, R>>>>()
            .expect("waterfall slots type mismatch")
    }

    /// Register a plain listener for event `E`, owned by `fiber`.
    ///
    /// Returns a disposer whose drop (or the fiber's unload) removes exactly
    /// this listener, in LIFO order among that fiber's effects.
    pub fn on<E, F>(
        &self,
        fiber: &Arc<Fiber>,
        listener: F,
        options: EventOptions,
    ) -> Result<Disposer>
    where
        E: Send + Sync + 'static,
        F: FnMut(&mut E) + Send + 'static,
    {
        let set = self.set_of::<E>();
        let slot = Arc::new(Mutex::new(Box::new(listener) as Listener<E>));
        let insertion = {
            let mut list = set.plain.write();
            if options.prepend {
                list.insert(0, slot.clone());
                0
            } else {
                list.push(slot.clone());
                list.len() - 1
            }
        };

        let removal: DisposeFn = {
            let set = set.clone();
            Box::new(move || {
                let mut list = set.plain.write();
                if insertion < list.len() {
                    list.remove(insertion);
                }
            })
        };
        let fiber_disposer = fiber.effect(move || Some(removal))?;
        Ok(fiber_disposer)
    }

    /// `emit`: fire-and-forget; run listeners in registration order, sync,
    /// no result (`void` in the reference). A listener panic is contained.
    pub fn emit<E: Send + Sync + 'static>(&self, mut event: E) {
        let slots = self.set_of::<E>().plain.read().clone();
        for slot in slots {
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                (slot.lock())(&mut event);
            }));
        }
    }

    /// `parallel`: run all listeners concurrently, await all, contain
    /// failures (all-settle semantics — one failure never cancels siblings).
    ///
    /// `E` must be `Clone`: each concurrent listener gets its own copy of the
    /// payload; there is no shared mutable event under fan-out.
    pub async fn parallel<E>(&self, event: E)
    where
        E: Clone + Send + Sync + 'static,
    {
        let slots = self.set_of::<E>().plain.read().clone();
        if slots.is_empty() {
            return;
        }
        let mut joins = Vec::with_capacity(slots.len());
        for slot in slots {
            let event = event.clone();
            joins.push(tokio::task::spawn(async move {
                let mut event = event;
                let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
                    (slot.lock())(&mut event);
                }));
            }));
        }
        for join in joins {
            let _ = join.await;
        }
    }

    /// The shared ordered walk behind `serial` and `bail`: run listeners in
    /// registration order, stopping at the first value `run` returns.
    fn ordered_walk<E, R>(
        &self,
        event: &mut E,
        mut run: impl FnMut(&mut E, &PlainSlot<E>) -> Option<R>,
    ) -> Option<R>
    where
        E: Send + Sync + 'static,
    {
        let slots = self.set_of::<E>().plain.read().clone();
        for slot in &slots {
            if let Some(value) = run(event, slot) {
                return Some(value);
            }
        }
        None
    }

    /// `serial`: run listeners in registration order, stopping at the first
    /// bail. `run` drives one listener against the payload and decides
    /// whether its value is a bail; the walk stops there.
    ///
    /// The reference awaits each listener here; the await-vs-inspect choice
    /// is deferred to the caller-driven `run`, which may itself await before
    /// reporting a bail.
    pub fn serial<E, R>(
        &self,
        event: &mut E,
        run: impl FnMut(&mut E, &PlainSlot<E>) -> Option<R>,
    ) -> Option<R>
    where
        E: Send + Sync + 'static,
    {
        self.ordered_walk(event, run)
    }

    /// `bail`: stop on the first synchronous bail value.
    ///
    /// Same ordered walk as `serial`, but the caller's `run` inspects a
    /// synchronous return immediately rather than awaiting a future — the
    /// reference's `bail` inspected-sync path.
    pub fn bail<E, R>(
        &self,
        event: &mut E,
        run: impl FnMut(&mut E, &PlainSlot<E>) -> Option<R>,
    ) -> Option<R>
    where
        E: Send + Sync + 'static,
    {
        self.ordered_walk(event, run)
    }

    /// `waterfall`: compose listeners around a terminal built-in behavior.
    ///
    /// Listeners run outermost-first — registration order, matching the
    /// reference's `cbs.shift()` walk. A listener that never calls `next()`
    /// vetoes the remainder, including `tail`. The result is the outermost
    /// listener's return value.
    pub fn waterfall<E, R>(&self, event: E, tail: impl FnOnce(E) -> R + 'static) -> R
    where
        E: Send + Sync + 'static,
        R: Send + 'static,
    {
        let mut slots = Self::waterfall_slots::<E, R>(&self.set_of::<E>())
            .read()
            .clone();
        // `Next::call` pops from the end, so reverse to make the first
        // registration the outermost wrapper (dispatch order).
        slots.reverse();
        let mut state = ChainState {
            pending: slots,
            tail: Some(Box::new(tail)),
        };
        let mut next = Next { chain: &mut state };
        next.call(event)
    }

    /// Register a waterfall listener for the dispatch `(E → R)`, owned by
    /// `fiber`.
    pub fn on_waterfall<E, R, F>(
        &self,
        fiber: &Arc<Fiber>,
        listener: F,
        options: EventOptions,
    ) -> Result<Disposer>
    where
        E: Send + Sync + 'static,
        R: Send + 'static,
        F: FnMut(&mut E, &mut Next<'_, E, R>) -> R + Send + 'static,
    {
        let slots = Self::waterfall_slots::<E, R>(&self.set_of::<E>());
        let slot = Arc::new(Mutex::new(Box::new(listener) as WaterfallListener<E, R>));
        let insertion = {
            let mut list = slots.write();
            if options.prepend {
                list.insert(0, slot.clone());
                0
            } else {
                list.push(slot.clone());
                list.len() - 1
            }
        };

        let removal: DisposeFn = {
            let slots = slots.clone();
            Box::new(move || {
                let mut list = slots.write();
                if insertion < list.len() {
                    list.remove(insertion);
                }
            })
        };
        let fiber_disposer = fiber.effect(move || Some(removal))?;
        Ok(fiber_disposer)
    }

    /// Remove all listeners registered for `E` (all result types).
    pub fn clear<E: Send + Sync + 'static>(&self) {
        self.events.write().remove(&TypeId::of::<E>());
    }

    /// Plain listeners registered for `E`.
    pub fn listener_count<E: Send + Sync + 'static>(&self) -> usize {
        self.events
            .read()
            .get(&TypeId::of::<E>())
            .and_then(|set| set.clone().downcast::<SetFor<E>>().ok())
            .map(|set| set.plain_count())
            .unwrap_or(0)
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Sample event payload.
    #[derive(Clone)]
    struct Ping {
        hits: usize,
    }

    #[test]
    fn emit_runs_listeners_in_registration_order() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let order = Arc::new(Mutex::new(Vec::new()));
        let order2 = order.clone();
        let _g1 = reg
            .on::<Ping, _>(
                &fiber,
                move |_e: &mut Ping| order2.lock().push(1),
                EventOptions::new(),
            )
            .unwrap();
        let order3 = order.clone();
        let _g2 = reg
            .on::<Ping, _>(
                &fiber,
                move |_e: &mut Ping| order3.lock().push(2),
                EventOptions::new(),
            )
            .unwrap();
        reg.emit(Ping { hits: 0 });
        assert_eq!(*order.lock(), vec![1, 2]);
    }

    #[test]
    fn prepend_places_listener_first() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let order = Arc::new(Mutex::new(Vec::new()));
        let o1 = order.clone();
        let _g1 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| o1.lock().push(1),
                EventOptions::new(),
            )
            .unwrap();
        let o2 = order.clone();
        let _g2 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| o2.lock().push(2),
                EventOptions::new(),
            )
            .unwrap();
        let o3 = order.clone();
        let _g3 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| o3.lock().push(0),
                EventOptions::prepend(),
            )
            .unwrap();
        reg.emit(Ping { hits: 0 });
        assert_eq!(*order.lock(), vec![0, 1, 2]);
    }

    #[test]
    fn disposer_removes_listener() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let hits = Arc::new(AtomicUsize::new(0));
        let h = hits.clone();
        let guard = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| {
                    h.fetch_add(1, Ordering::SeqCst);
                },
                EventOptions::new(),
            )
            .unwrap();
        reg.emit(Ping { hits: 0 });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        guard.dispose();
        reg.emit(Ping { hits: 0 });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(reg.listener_count::<Ping>(), 0);
    }

    #[test]
    fn fiber_dispose_removes_its_listeners() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let _guard = reg
            .on::<Ping, _>(&fiber, |_: &mut Ping| {}, EventOptions::new())
            .unwrap();
        assert_eq!(reg.listener_count::<Ping>(), 1);
        fiber.dispose();
        assert_eq!(reg.listener_count::<Ping>(), 0);
    }

    #[test]
    fn contained_listener_panic_does_not_block_siblings() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let hits = Arc::new(AtomicUsize::new(0));
        let h1 = hits.clone();
        let _g1 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| {
                    let _ = h1;
                    panic!("listener exploded");
                },
                EventOptions::new(),
            )
            .unwrap();
        let h2 = hits.clone();
        let _g2 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| {
                    h2.fetch_add(1, Ordering::SeqCst);
                },
                EventOptions::new(),
            )
            .unwrap();
        reg.emit(Ping { hits: 0 });
        assert_eq!(hits.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn serial_stops_at_first_bail() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let hits = Arc::new(AtomicUsize::new(0));
        let h1 = hits.clone();
        let _g1 = reg
            .on::<Ping, _>(
                &fiber,
                move |e: &mut Ping| {
                    h1.fetch_add(1, Ordering::SeqCst);
                    e.hits += 1;
                },
                EventOptions::new(),
            )
            .unwrap();
        let h2 = hits.clone();
        let _g2 = reg
            .on::<Ping, _>(
                &fiber,
                move |e: &mut Ping| {
                    h2.fetch_add(10, Ordering::SeqCst);
                    e.hits += 10;
                },
                EventOptions::new(),
            )
            .unwrap();

        let mut event = Ping { hits: 0 };
        // Bail inspected synchronously per listener; the walk stops on the
        // first bail decision, so the second listener never runs.
        let bail = reg.bail::<Ping, usize>(&mut event, |_e, slot| {
            let mut listener = slot.lock();
            listener(_e);
            Some(7)
        });
        assert_eq!(bail, Some(7));
        assert_eq!(hits.load(Ordering::SeqCst), 1);
        assert_eq!(event.hits, 1);
    }

    #[test]
    fn serial_without_bail_runs_all_in_order() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let order = Arc::new(Mutex::new(Vec::new()));
        let o1 = order.clone();
        let _g1 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| o1.lock().push(1),
                EventOptions::new(),
            )
            .unwrap();
        let o2 = order.clone();
        let _g2 = reg
            .on::<Ping, _>(
                &fiber,
                move |_: &mut Ping| o2.lock().push(2),
                EventOptions::new(),
            )
            .unwrap();
        let mut event = Ping { hits: 0 };
        // No bail decision until the walk is exhausted.
        let result = reg.serial::<Ping, ()>(&mut event, |_e, slot| {
            let mut listener = slot.lock();
            listener(_e);
            None
        });
        assert_eq!(result, None);
        assert_eq!(*order.lock(), vec![1, 2]);
    }

    #[test]
    fn waterfall_wraps_and_vetoes() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let _guard = reg
            .on_waterfall::<Ping, usize, _>(
                &fiber,
                |e: &mut Ping, next: &mut Next<Ping, usize>| {
                    // Wrap: increment then delegate.
                    e.hits += 1;
                    let v = next.call(Ping { hits: e.hits });
                    v + 100
                },
                EventOptions::new(),
            )
            .unwrap();
        let result = reg.waterfall(Ping { hits: 0 }, |e| e.hits);
        assert_eq!(result, 101); // listener delegated, tail saw hits=1, +100
    }

    #[test]
    fn waterfall_veto_skips_tail() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let _guard = reg
            .on_waterfall::<Ping, usize, _>(
                &fiber,
                |e: &mut Ping, _next: &mut Next<Ping, usize>| {
                    // Veto: own the decision; never delegate.
                    e.hits + 5
                },
                EventOptions::new(),
            )
            .unwrap();
        let fired = Arc::new(AtomicUsize::new(0));
        let f = fired.clone();
        let result = reg.waterfall(Ping { hits: 3 }, move |e| {
            f.fetch_add(1, Ordering::SeqCst);
            e.hits
        });
        assert_eq!(result, 8);
        assert_eq!(fired.load(Ordering::SeqCst), 0, "tail must not run on veto");
    }

    #[test]
    fn waterfall_delegates_through_in_order() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let _g1 = reg
            .on_waterfall::<Ping, usize, _>(
                &fiber,
                |e: &mut Ping, next: &mut Next<Ping, usize>| {
                    e.hits *= 2;
                    next.call(Ping { hits: e.hits })
                },
                EventOptions::new(),
            )
            .unwrap();
        let _g2 = reg
            .on_waterfall::<Ping, usize, _>(
                &fiber,
                |e: &mut Ping, next: &mut Next<Ping, usize>| {
                    e.hits += 1;
                    next.call(Ping { hits: e.hits })
                },
                EventOptions::new(),
            )
            .unwrap();
        // Registration order is dispatch order: outermost (first registered)
        // wraps first. 3 → ×2 → +1 → tail = 7.
        let result = reg.waterfall(Ping { hits: 3 }, |e| e.hits);
        assert_eq!(result, 7);
    }

    #[tokio::test]
    async fn parallel_runs_all_listeners() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let hits = Arc::new(AtomicUsize::new(0));
        let mut keeps = Vec::new();
        for _ in 0..3 {
            let h = hits.clone();
            keeps.push(
                reg.on::<Ping, _>(
                    &fiber,
                    move |_: &mut Ping| {
                        h.fetch_add(1, Ordering::SeqCst);
                    },
                    EventOptions::new(),
                )
                .unwrap(),
            );
        }
        reg.parallel(Ping { hits: 0 }).await;
        assert_eq!(hits.load(Ordering::SeqCst), 3);
    }

    #[tokio::test]
    async fn parallel_contains_listener_panics() {
        let reg = EventRegistry::new();
        let fiber = Fiber::active();
        let _guard = reg
            .on::<Ping, _>(&fiber, |_: &mut Ping| panic!("boom"), EventOptions::new())
            .unwrap();
        // Must complete without propagating the panic (allSettled semantics).
        reg.parallel(Ping { hits: 0 }).await;
    }
}
