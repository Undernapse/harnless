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
//! * Cleanups run at most once, whether reached by explicit dispose, guard
//!   drop, or fiber teardown.
//! * Fiber teardown runs disposers in reverse registration order (LIFO).
//! * One broken disposer cannot take down the teardown of the rest.
//!
//! # Guard semantics (the Rust translation of Cordis's disposer)
//!
//! Cordis hands back a disposer *function* the caller invokes. The RAII
//! translation deliberately differs on drop:
//!
//! * **Dropping [`Disposer`] detaches ownership** — the effect stays
//!   registered and unwinds when its fiber unloads. `ctx.provide(svc)?;`
//!   inside a plugin body therefore keeps the registration alive for the
//!   fiber's lifetime; a discarded handle means "the fiber owns it now",
//!   never "unregister me".
//! * **`Disposer::dispose()` runs the cleanup now** — the explicit, one-shot
//!   early-teardown form.

use std::sync::atomic::{AtomicU8, Ordering};
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

/// Handle over one registered reversible effect.
///
/// Dropping the handle **detaches ownership** (the effect unwinds with its
/// fiber); calling [`Disposer::dispose`] runs the cleanup immediately.
#[derive(Debug, Clone)]
pub struct Disposer {
    fiber: Arc<Fiber>,
    idx: usize,
    state: Arc<AtomicU8>,
}

const LIVE: u8 = 0;
const DISPOSED: u8 = 1;

impl Disposer {
    /// Run this effect's cleanup now, exactly once.
    ///
    /// Consumes the handle; further clones are inert. Idempotent alongside
    /// fiber teardown: whichever path runs first wins.
    pub fn dispose(self) {
        if self.state.swap(DISPOSED, Ordering::SeqCst) == LIVE {
            self.fiber.take_and_run(self.idx);
        }
    }
}

impl Drop for Disposer {
    fn drop(&mut self) {
        // Detach: the effect stays in the fiber's LIFO list and runs at fiber
        // unload. No cleanup runs here — that is the deliberate semantics.
    }
}

/// A cleanup closure that undoes one registration.
pub type DisposeFn = Box<dyn FnMut() + Send>;

struct FiberInner {
    state: FiberState,
    /// Registered cleanups in insertion order; `None` marks a vacated slot.
    /// Reverse iteration yields LIFO teardown.
    effects: Vec<Option<DisposeFn>>,
}

/// A lifecycle owner of reversible effects.
///
/// Fibers are always created shared: [`Fiber::active`] / [`Fiber::pending`]
/// return `Arc<Fiber>` because plugins, contexts, and disposers all hold the
/// same instance.
#[derive(Clone)]
pub struct Fiber {
    inner: Arc<Mutex<FiberInner>>,
}

impl core::fmt::Debug for Fiber {
    fn fmt(&self, f: &mut core::fmt::Formatter<'_>) -> core::fmt::Result {
        f.debug_struct("Fiber")
            .field("state", &self.state())
            .field("live_effects", &self.effect_count())
            .finish()
    }
}

impl Fiber {
    /// Create an `Active` fiber, shared.
    pub fn active() -> Arc<Fiber> {
        Arc::new(Self {
            inner: Arc::new(Mutex::new(FiberInner {
                state: FiberState::Active,
                effects: Vec::new(),
            })),
        })
    }

    /// Create a `Pending` fiber, shared.
    pub fn pending() -> Arc<Fiber> {
        Arc::new(Self {
            inner: Arc::new(Mutex::new(FiberInner {
                state: FiberState::Pending,
                effects: Vec::new(),
            })),
        })
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
    /// error otherwise (`INACTIVE_EFFECT` while unloading, `INACTIVE_FIBER`
    /// once disposed).
    pub fn assert_active(&self) -> Result<()> {
        match self.state() {
            FiberState::Disposed => Err(RuntimeError::inactive_fiber()),
            FiberState::Unloading => Err(RuntimeError::inactive_effect()),
            _ => Ok(()),
        }
    }

    /// Register a reversible effect.
    ///
    /// The body runs immediately; the cleanup it returns is collected and run
    /// (in reverse order with the rest) when the fiber unloads, or earlier via
    /// an explicit [`Disposer::dispose`]. The slot is reserved before the
    /// body runs so a reentrant unload finds the effect even while its setup
    /// is in flight. Refuses registration while unloading or disposed.
    pub fn effect<F>(&self, body: F) -> Result<Disposer>
    where
        F: FnOnce() -> Option<DisposeFn>,
    {
        self.assert_active()?;

        // Reserve the slot *before* running the body (Cordis hardening: the
        // owner-list wrapper registers before setup executes).
        let idx = {
            let mut guard = self.inner.lock();
            guard.effects.push(None);
            guard.effects.len() - 1
        };

        let cleanup = body();
        if let Some(cleanup) = cleanup {
            let mut guard = self.inner.lock();
            guard.effects[idx] = Some(cleanup);
        }

        Ok(Disposer {
            fiber: Arc::new(Fiber {
                inner: self.inner.clone(),
            }),
            idx,
            state: Arc::new(AtomicU8::new(LIVE)),
        })
    }

    /// Take the cleanup at `idx` (vacating the slot) and run it once.
    fn take_and_run(&self, idx: usize) {
        let cleanup = {
            let mut guard = self.inner.lock();
            let slot = guard.effects.get_mut(idx);
            slot.and_then(|s| s.take())
        };
        // A registration whose slot was already vacated by fiber teardown is
        // a no-op: whichever path reached it first owns the single run.
        if let Some(mut cleanup) = cleanup {
            cleanup();
        }
    }

    /// Tear down the fiber: run all remaining effects in LIFO order and mark
    /// the fiber disposed.
    ///
    /// Idempotent: a second call is a no-op. Each disposer is isolated, so a
    /// panicking cleanup cannot break the teardown of the rest.
    pub fn dispose(&self) -> FiberState {
        if self.state() == FiberState::Disposed {
            return FiberState::Disposed;
        }
        let prev = self.set_state(FiberState::Unloading);
        if prev == FiberState::Disposed {
            // A concurrent or racing caller finished teardown first.
            self.set_state(FiberState::Disposed);
            return prev;
        }

        // Snapshot remaining cleanups in reverse (LIFO) order, vacating every
        // slot so explicit disposers racing teardown do not double-run.
        let cleanups: Vec<DisposeFn> = {
            let mut guard = self.inner.lock();
            let mut reversed: Vec<DisposeFn> = Vec::with_capacity(guard.effects.len());
            for slot in guard.effects.iter_mut().rev() {
                if let Some(cleanup) = slot.take() {
                    reversed.push(cleanup);
                }
            }
            reversed
        };

        for cleanup in cleanups {
            // Containment: one broken disposer never breaks teardown.
            let _ = std::panic::catch_unwind(std::panic::AssertUnwindSafe(cleanup));
        }

        self.set_state(FiberState::Disposed);
        prev
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::AtomicUsize;

    #[test]
    fn effect_runs_cleanup_on_explicit_dispose() {
        let fiber = Fiber::active();
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
        guard.dispose();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn guard_drop_detaches_and_fiber_unload_still_runs_cleanup() {
        let fiber = Fiber::active();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let guard = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        drop(guard); // detach, not dispose
        assert_eq!(ran.load(Ordering::SeqCst), 0, "drop must not run cleanup");
        assert_eq!(fiber.effect_count(), 1);
        fiber.dispose();
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn disposer_dispose_is_single_shot_across_clones() {
        let fiber = Fiber::active();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let guard = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        let clone = guard.clone();
        guard.dispose();
        clone.dispose(); // idempotent no-op
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn fiber_dispose_runs_effects_lifo() {
        let fiber = Fiber::active();
        let order = Arc::new(Mutex::new(Vec::new()));
        let a = order.clone();
        let _ga = fiber
            .effect(move || Some(Box::new(move || a.lock().push("a")) as DisposeFn))
            .unwrap();
        let b = order.clone();
        let _gb = fiber
            .effect(move || Some(Box::new(move || b.lock().push("b")) as DisposeFn))
            .unwrap();
        fiber.dispose();
        assert_eq!(*order.lock(), vec!["b", "a"]);
    }

    #[test]
    fn fiber_dispose_is_idempotent_and_marks_disposed() {
        let fiber = Fiber::active();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let _guard = fiber
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
    fn refuse_effect_while_unloading_or_disposed() {
        let fiber = Fiber::active();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let _guard = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        fiber.dispose();
        let late = fiber.effect(|| None);
        assert_eq!(late.unwrap_err().code, "INACTIVE_FIBER");
        assert_eq!(ran.load(Ordering::SeqCst), 1);
    }

    #[test]
    fn effect_with_no_cleanup_is_legal() {
        let fiber = Fiber::pending();
        let _guard = fiber.effect(|| None).unwrap();
        assert_eq!(fiber.effect_count(), 0);
        fiber.dispose();
        assert_eq!(fiber.state(), FiberState::Disposed);
    }

    #[test]
    fn panicking_disposer_does_not_break_teardown() {
        let fiber = Fiber::active();
        let ran = Arc::new(AtomicUsize::new(0));
        let r = ran.clone();
        let _g1 = fiber
            .effect(move || {
                Some(Box::new(move || {
                    r.fetch_add(1, Ordering::SeqCst);
                }) as DisposeFn)
            })
            .unwrap();
        let _g2 = fiber
            .effect(|| Some(Box::new(|| panic!("cleanup exploded")) as DisposeFn))
            .unwrap();
        fiber.dispose(); // must not panic; both slots processed
        assert_eq!(ran.load(Ordering::SeqCst), 1);
        assert_eq!(fiber.state(), FiberState::Disposed);
    }
}
