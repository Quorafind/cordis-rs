//! Reaching Boa's `Context` from inside the engine.
//!
//! Boa hands a native function its `&mut Context` as an argument; the engine, generic over its
//! host, has no parameter for it. Every way into the engine from JavaScript — an op, a promise
//! reaction, a drain job — runs inside [`enter`], which records the context it was given for
//! the length of the call; [`with`] reaches the innermost one.
//!
//! Soundness rests on one rule the binding keeps: a [`with`] closure never re-enters the engine
//! directly. Re-entry goes through JavaScript (a host call), and Boa hands the native it lands
//! in a fresh reborrow, which [`enter`] records on top — so at any moment only the innermost
//! recorded reference is being used.

use std::cell::RefCell;

use boa_engine::Context;

thread_local! {
    static STACK: RefCell<Vec<*mut Context>> = const { RefCell::new(Vec::new()) };
}

struct Guard;

impl Drop for Guard {
    fn drop(&mut self) {
        STACK.with(|stack| stack.borrow_mut().pop());
    }
}

/// Runs `f` with `context` recorded as the one the engine reaches.
pub(crate) fn enter<R>(context: &mut Context, f: impl FnOnce() -> R) -> R {
    STACK.with(|stack| stack.borrow_mut().push(context as *mut Context));
    let _guard = Guard;
    let result = f();
    cordis::task::request_drain();
    result
}

/// Whether some engine call is in progress on this thread.
pub(crate) fn active() -> bool {
    STACK.with(|stack| !stack.borrow().is_empty())
}

/// Runs `f` with the innermost recorded context.
///
/// # Panics
/// Outside every [`enter`]: the engine was reached without going through the binding.
pub(crate) fn with<R>(f: impl FnOnce(&mut Context) -> R) -> R {
    let pointer = STACK.with(|stack| stack.borrow().last().copied());
    let pointer = pointer.expect("cordis-verve: the engine was reached outside a native call");
    // SAFETY: the pointer was recorded by `enter` from a live `&mut Context` whose frame is
    // still on the stack (the guard pops it before that frame returns), and per the module
    // rule nothing else uses that reference while this closure runs.
    let context = unsafe { &mut *pointer };
    f(context)
}

/// [`with`], or `None` outside every native call.
pub(crate) fn try_with<R>(f: impl FnOnce(&mut Context) -> R) -> Option<R> {
    if !active() {
        return None;
    }
    Some(with(f))
}
