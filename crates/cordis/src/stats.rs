//! Live-object counts, for finding what outlives its use (a leak test reads them).

use std::cell::Cell;

thread_local! {
    pub(crate) static FIBERS: Cell<isize> = const { Cell::new(0) };
    pub(crate) static EFFECTS: Cell<isize> = const { Cell::new(0) };
    pub(crate) static RUNTIMES: Cell<isize> = const { Cell::new(0) };
    pub(crate) static RELEASED: Cell<isize> = const { Cell::new(0) };
}

pub(crate) fn add(counter: &'static std::thread::LocalKey<Cell<isize>>, delta: isize) {
    let _ = counter.try_with(|c| c.set(c.get() + delta));
}

/// What is alive on this thread: fibers, effects, runtimes, and tasks still pending.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub struct Stats {
    pub fibers: isize,
    pub effects: isize,
    pub runtimes: isize,
    pub pending_tasks: usize,
    /// Fibers whose disposal finished.
    pub released: isize,
}

pub fn stats() -> Stats {
    Stats {
        fibers: FIBERS.with(Cell::get),
        effects: EFFECTS.with(Cell::get),
        runtimes: RUNTIMES.with(Cell::get),
        pending_tasks: crate::task::pending(),
        released: RELEASED.with(Cell::get),
    }
}
