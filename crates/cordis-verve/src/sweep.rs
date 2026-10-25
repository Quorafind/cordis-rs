//! Dropping Rust-held JavaScript values safely when the collector frees their owner.
//!
//! Boa counts a `Gc` held outside its heap as a root, and un-counts it when the `Gc` is dropped
//! — except while the collector is freeing objects (`boa_gc::finalizer_safe()` is false then),
//! when the decrement is skipped on purpose: a traced `Gc` inside a freed object points at
//! nodes freed in the same sweep. A `JsValue` held by *native* data of a freed object — a
//! handle's engine object, the captures of a native closure — is not traced, so the skipped
//! decrement leaves its target counted as rooted for good: it, and everything it reaches, is
//! never collected. (A disposed plugin's closures were exactly that.)
//!
//! [`SweepSafe`] wraps such data. Dropped during a sweep, it parks its contents in a
//! thread-local graveyard instead; [`bury`] drops them later, outside the collector — every
//! native entry ([`crate::cx::enter`]) does.

use std::any::Any;
use std::cell::RefCell;
use std::mem::ManuallyDrop;
use std::ops::Deref;

thread_local! {
    static GRAVEYARD: RefCell<Vec<Box<dyn Any>>> = const { RefCell::new(Vec::new()) };
}

/// A value whose drop is deferred when the collector is freeing its owner.
pub(crate) struct SweepSafe<T: 'static>(ManuallyDrop<T>);

impl<T: 'static> SweepSafe<T> {
    pub(crate) fn new(value: T) -> Self {
        Self(ManuallyDrop::new(value))
    }
}

impl<T: 'static> Deref for SweepSafe<T> {
    type Target = T;

    fn deref(&self) -> &T {
        &self.0
    }
}

impl<T: 'static> Drop for SweepSafe<T> {
    fn drop(&mut self) {
        // SAFETY: `self.0` is taken exactly once, here, and never touched again.
        let value = unsafe { ManuallyDrop::take(&mut self.0) };
        if boa_gc::finalizer_safe() {
            drop(value);
            return;
        }
        let parked = GRAVEYARD.try_with(|graveyard| {
            graveyard.borrow_mut().push(Box::new(value));
        });
        // Thread teardown: nothing left to leak into.
        let _ = parked;
    }
}

/// Drops what sweeps parked. Called outside the collector.
pub(crate) fn bury() {
    if !boa_gc::finalizer_safe() {
        return;
    }
    loop {
        let parked = GRAVEYARD
            .try_with(|graveyard| std::mem::take(&mut *graveyard.borrow_mut()))
            .unwrap_or_default();
        if parked.is_empty() {
            return;
        }
        drop(parked);
    }
}
