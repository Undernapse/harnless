//! Fibers and reversible effects.
//!
//! A fiber owns the reversible effects (disposers) registered by one mounted
//! plugin. Unloading a fiber unwinds exactly its own effects in LIFO order, so
//! a plugin's tools, listeners, schemas, and prompt sections disappear
//! together when it is removed — the mechanical guarantee behind "everything
//! is a plugin".
//!
//! Mirroring Cordis's hardening, this module enforces the invariants the dsh
//! team had to harden against (ticket 03-Q3, research 02):
//!
//! * Creation of a new effect is refused while the fiber is unloading or
//!   disposed (`INACTIVE_EFFECT` / `INACTIVE_FIBER`).
//! * An effect's slot is reserved *before* its setup body runs, so a reentrant
//!   unload that starts during setup can still find and join its cleanup.
//! * Disposers are single-shot: calling (or dropping) one twice is a no-op.
//! * Fiber teardown runs disposers in reverse registration order (LIFO).

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::Arc;

use parking_lot::Mutex;

use crate::error::{Result, RuntimeError};

/// Lifecycle state of a fiber.
///
/// `Pending` — waiting for required services; `Loading` — the plugin body is
/// running; `Active` — loaded and providing; `Failed` — the body threw;
/// `Unloading` — disposers are running; `Disposed` — the fiber was removed and
/// cannot be restarted.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum FiberState {
    Pending,
    Loading,
    Active,
    Failed,
    Unloading,
    Disposed,
}

/// A registered reversible effect.
///
/// Historically Cordis returns a disposer *function*; here the effect body
/// returns an optional cleanup closure and the runtime hands back a guard that
/// unwinds it. Dropping the guard runs the cleanup once; calling [`dispose`]
/// is the explicit, awaiting form.
///
/// [`dispose`]: Disposer::dispose
#[derive(Clone)]
pub struct Disposer {
    fiber: Fiber,
    idx: usize,
    disposed: Arc<AtomicBool>,
}

impl Disposer {
    /// Run this disposer's cleanup, if it has not already run.
    ///
    /// The guard is consumed; further use is a no-op. This is the RAII form of
    /// unwinding a single registration.
    pub fn dispose(self) {
        drop(self)
    }
}

impl Drop for Disposer {
    fn drop(&mut self) {
        if !self.disposed.swap(true, Ordering::SeqCst) {
            self.fiber.take_and_run(self.idx);
        }
    }
}

/// The effect body of a fiber: a closure that installs a resource and returns
/// the cleanup that undoes it (or `None` when there is nothing to undo).
pub type Effect = Box<dyn FnOnce() -> Option<DisposeFn> + Send>;

/// A cleanup closure that undoes one registration.
pub type DisposeFn = Box<dyn FnMut() + Send>;

struct FiberInner {
    state: FiberState,
    /// Registered cleanups in insertion order; `None` marks a vacated slot.
    /// Reverse iteration yields LIFO teardown.
    effects: Vec<Option<DisposeFn>>,
}

/// A lifecycle owner of reversible effects.
#[derive(Clone)]
pub struct Fiber {
    inner: Arc<Mutex<FiberInner>>,
}

impl Default for Fiber {
    fn default() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FiberInner {
                state: FiberState::Active,
                effects: Vec::new(),
            })),
        }
    }
}

impl Fiber {
    /// Create a fiber in the `Pending` state.
    pub fn pending() -> Self {
        Self {
            inner: Arc::new(Mutex::new(FiberInner {
                state: FiberState::Pending,
                effects: Vec::new(),
            })),
        }
    }

    /// The fiber's current lifecycle state.
    pub fn state(&self) -> FiberState {
        self.inner.lock().state
    }

    /// Number of live effects this fiber owns.
    pub fn effect_count(&self) -> usize {
        self.inner
            .lock()
            .effects
            .iter()
            .filter(|slot| slot.is_some())
            .count()
    }

    /// Set the lifecycle state, returning the previous value.
    pub fn set_state(&self, state: FiberState) -> FiberState {
        let mut guard = self.inner.lock();
        let old = guard.state;
        guard.state = state;
        old
    }

    /// Return `Ok` while the fiber may register new effects, or the stable
    /// error otherwise.
    pub fn assert_active(&self) -> Result<()> {
        registration_error(self.state()).map_or(Ok(()), Err)
    }

    /// Register a reversible effect.
    ///
    /// The body runs immediately; the cleanup it returns is collected and run
    /// (in reverse order) when the fiber unloads or when the returned guard is
    /// dropped, whichever comes first. The guard is single-shot.
    ///
    /// Refuses to register while the fiber is unloading or disposed.
    pub fn effect<F>(&self, body: F) -> Result<Disposer>
    where
        F: FnOnce() -> Option<DisposeFn> + Send + 'static,
    {
        // Reserve the slot *before* running the body so a reentrant unload can
        // find this effect even while its setup is in flight.
        let idx = {
            let mut guard = self.inner.lock();
            if let Some(error) = registration_error(guard.state) {
                return Err(error);
            }
            guard.effects.push(None);
            guard.effects.len() - 1
        };

        let cleanup = body();
        let disposed = Arc::new(AtomicBool::new(false));
        if let Some(mut cleanup) = cleanup {
            let mut guard = self.inner.lock();
            if matches!(guard.state, FiberState::Unloading | FiberState::Disposed) {
                // Teardown raced or re-entered setup. It already observed the
                // reserved slot, so this cleanup must run here.
                disposed.store(true, Ordering::SeqCst);
                drop(guard);
                cleanup();
            } else {
                guard.effects[idx] = Some(cleanup);
            }
        }

        Ok(Disposer {
            fiber: self.clone(),
            idx,
            disposed,
        })
    }

    /// Take the cleanup at `idx` (marking the slot vacated) and run it.
    ///
    /// Used by guard-drop so disposes of individual effects happen exactly
    /// once and no fiber teardown runs them a second time.
    fn take_and_run(&self, idx: usize) {
        let cleanup = {
            let mut guard = self.inner.lock();
            guard.effects.get_mut(idx).and_then(Option::take)
        };
        if let Some(mut cleanup) = cleanup {
            cleanup();
        }
    }

    /// Tear down the fiber: run all remaining effects in LIFO order and mark
    /// the fiber disposed.
    ///
    /// Returns the previous state. Idempotent: a second call is a no-op.
    pub fn dispose(&self) -> FiberState {
        let prev = {
            let mut guard = self.inner.lock();
            if matches!(guard.state, FiberState::Unloading | FiberState::Disposed) {
                return guard.state;
            }
            let prev = guard.state;
            guard.state = FiberState::Unloading;
            prev
        };

        // Snapshot the remaining cleanups in reverse (LIFO) order, vacating
        // every slot so guard-drops racing teardown do not double-run.
        let cleanups: Vec<DisposeFn> = {
            let mut guard = self.inner.lock();
            let mut reversed: Vec<DisposeFn> = Vec::new();
            for slot in guard.effects.iter_mut().rev() {
                if let Some(cleanup) = slot.take() {
                    reversed.push(cleanup);
                }
            }
            reversed
        };

        for cleanup in cleanups {
            // One broken observer must not take down teardown of the rest;
            // isolate each disposer failure (containment, decision 03).
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup));
        }

        self.set_state(FiberState::Disposed);
        prev
    }
}

fn registration_error(state: FiberState) -> Option<RuntimeError> {
    match state {
        FiberState::Disposed => Some(RuntimeError::inactive_fiber()),
        FiberState::Unloading => Some(RuntimeError::inactive_effect()),
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn effect_runs_cleanup_on_guard_drop() {
        let fiber = Fiber::pending();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let guard = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        assert_eq!(ran.load(Ordering::SeqCst), 0);
        drop(guard);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disposer_is_single_shot() {
        let fiber = Fiber::pending();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let guard = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        let guard2 = guard.clone();
        drop(guard);
        drop(guard2);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fiber_dispose_runs_effects_lifo() {
        let fiber = Fiber::pending();
        let order = Arc::new(Mutex::new(Vec::new()));
        let a = order.clone();
        let b = order.clone();
        let _a_guard = fiber
            .effect(move || Some(Box::new(move || a.lock().push("a")) as DisposeFn))
            .unwrap();
        let _b_guard = fiber
            .effect(move || Some(Box::new(move || b.lock().push("b")) as DisposeFn))
            .unwrap();
        fiber.dispose();
        assert_eq!(*order.lock(), vec!["b", "a"]);
    }

    #[test]
    fn fiber_dispose_is_idempotent_and_marks_disposed() {
        let fiber = Fiber::pending();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        fiber.dispose();
        fiber.dispose();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    #[test]
    fn refuse_effect_while_unloading() {
        let fiber = Fiber::pending();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        fiber.dispose();
        // Unloading/Disposed now; a late effect must be refused.
        let error = match fiber.effect(|| None) {
            Ok(_) => panic!("disposed fiber accepted a late effect"),
            Err(error) => error,
        };
        assert_eq!(error.code, "INACTIVE_FIBER");
    }

    #[test]
    fn effect_with_no_cleanup_is_legal() {
        let fiber = Fiber::pending();
        let guard = fiber.effect(|| None).unwrap();
        drop(guard);
        assert_eq!(fiber.state(), FiberState::Pending);
    }

    #[test]
    fn reentrant_dispose_during_setup_cannot_leak_the_cleanup() {
        let fiber = Fiber::pending();
        let disposer = fiber.clone();
        let ran = Arc::new(AtomicUsize::new(0));
        let marker = ran.clone();
        let guard = fiber
            .effect(move || {
                disposer.dispose();
                Some(Box::new(move || {
                    marker.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        assert_eq!(fiber.state(), FiberState::Disposed);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        drop(guard);
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }
}
