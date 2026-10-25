//! Eager local tasks: Rust futures that behave as JavaScript `async` functions do.
//!
//! Cordis is written in terms of `async` functions whose *synchronous prefix* is part of the
//! semantics: `_reload` snapshots a fiber's store before its first `await`, `_unload` clears the
//! disposables before it awaits any of them, a provider's disposer deletes its impl and
//! notifies before it waits for the fibers it woke. A Rust `async` block does nothing until it
//! is polled, so [`spawn`] polls it once, immediately: the task runs exactly as far as the JS
//! function would have before handing back its promise.
//!
//! Later polls happen when a task is woken. Wakes go to a thread-local ready queue, which the
//! host drains with [`run_ready`]:
//!
//! - natively, [`run_until_stalled`] — or any loop that calls it;
//! - on a JavaScript engine, as a microtask: the host installs a [`set_scheduler`] hook, called
//!   whenever the queue goes from empty to non-empty, that queues one drain job. A drain polls
//!   only what was ready when it started, so a task that yields (`await Promise.resolve()`)
//!   continues after the microtasks queued meanwhile, as in JavaScript.
//!
//! A task may carry a [`Scope`]: the host's async context (an `AsyncLocalStorage` snapshot on
//! Verve) captured when the task was spawned, entered around every later poll.
//!
//! Wakes from another thread are queued for the task's own thread and reported to the notifier
//! registered for it with [`set_remote_notifier`], so a host's loop can wake and drain.

use std::cell::{Cell, RefCell};
use std::collections::{HashMap, VecDeque};
use std::fmt;
use std::future::Future;
use std::pin::Pin;
use std::rc::{Rc, Weak};
use std::sync::{Arc, Mutex, OnceLock};
use std::task::{Context, Poll, Wake, Waker};
use std::thread::ThreadId;

/// A boxed, local, `'static` future.
pub type LocalBoxFuture<T> = Pin<Box<dyn Future<Output = T>>>;

/// The host's async context, entered around each poll of a task that captured it.
pub trait Scope {
    /// Runs `f` inside this scope.
    fn enter(&self, f: &mut dyn FnMut());
}

trait Runnable {
    fn run(self: Rc<Self>);
}

thread_local! {
    static TASKS: RefCell<HashMap<u64, Weak<dyn Runnable>>> = RefCell::new(HashMap::new());
    static READY: RefCell<VecDeque<u64>> = const { RefCell::new(VecDeque::new()) };
    static NEXT_ID: Cell<u64> = const { Cell::new(1) };
    static DRAIN_PENDING: Cell<bool> = const { Cell::new(false) };
    static SCHEDULER: RefCell<Option<Rc<dyn Fn() -> bool>>> = const { RefCell::new(None) };
}

type Notifier = Arc<dyn Fn() + Send + Sync>;

/// Wakes that arrived from other threads, per owning thread, and how to tell each owner.
fn remote() -> &'static Mutex<HashMap<ThreadId, (Vec<u64>, Option<Notifier>)>> {
    static REMOTE: OnceLock<Mutex<HashMap<ThreadId, (Vec<u64>, Option<Notifier>)>>> =
        OnceLock::new();
    REMOTE.get_or_init(|| Mutex::new(HashMap::new()))
}

/// Installs the hook asked to arrange a [`run_ready`] soon — a microtask, a loop turn — when
/// this thread's ready queue becomes non-empty. It answers whether it did; one that could not
/// (no engine to queue a job on just then) is asked again at the next wake. Without one, the
/// queue waits for an explicit [`run_ready`] or [`run_until_stalled`].
pub fn set_scheduler(scheduler: Option<Rc<dyn Fn() -> bool>>) {
    SCHEDULER.with(|slot| *slot.borrow_mut() = scheduler);
}

/// Registers how another thread tells this one that one of its tasks was woken.
pub fn set_remote_notifier(notifier: Option<Arc<dyn Fn() + Send + Sync>>) {
    let mut remote = remote().lock().unwrap_or_else(|e| e.into_inner());
    let entry = remote
        .entry(std::thread::current().id())
        .or_insert_with(|| (Vec::new(), None));
    entry.1 = notifier;
}

fn schedule_local(id: u64) {
    // A wake during thread teardown has nowhere to go, and nothing left to run it.
    let queued = READY.try_with(|ready| ready.borrow_mut().push_back(id));
    if queued.is_err() {
        return;
    }
    request_drain();
}

/// Asks the scheduler for a drain, unless one is already arranged.
pub fn request_drain() {
    let pending = DRAIN_PENDING.try_with(Cell::get).unwrap_or(true);
    let ready = READY
        .try_with(|ready| !ready.borrow().is_empty())
        .unwrap_or(false);
    if pending || !ready {
        return;
    }
    let scheduler = SCHEDULER.with(|slot| slot.borrow().clone());
    if let Some(scheduler) = scheduler {
        DRAIN_PENDING.with(|pending| pending.set(true));
        if !scheduler() {
            DRAIN_PENDING.with(|pending| pending.set(false));
        }
    }
}

/// Forgets a drain that was arranged but will never run (its engine is gone).
pub fn reset_drain() {
    DRAIN_PENDING.with(|pending| pending.set(false));
}

struct TaskWaker {
    id: u64,
    thread: ThreadId,
}

impl Wake for TaskWaker {
    fn wake(self: Arc<Self>) {
        self.wake_by_ref();
    }

    fn wake_by_ref(self: &Arc<Self>) {
        if std::thread::current().id() == self.thread {
            schedule_local(self.id);
            return;
        }
        let notifier = {
            let mut remote = remote().lock().unwrap_or_else(|e| e.into_inner());
            let entry = remote
                .entry(self.thread)
                .or_insert_with(|| (Vec::new(), None));
            entry.0.push(self.id);
            entry.1.clone()
        };
        if let Some(notifier) = notifier {
            notifier();
        }
    }
}

/// Whether anything is waiting to be polled on this thread.
pub fn has_ready() -> bool {
    READY.with(|ready| !ready.borrow().is_empty()) || has_remote()
}

fn has_remote() -> bool {
    let remote = remote().lock().unwrap_or_else(|e| e.into_inner());
    remote
        .get(&std::thread::current().id())
        .is_some_and(|(ids, _)| !ids.is_empty())
}

fn take_remote() {
    let ids = {
        let mut remote = remote().lock().unwrap_or_else(|e| e.into_inner());
        match remote.get_mut(&std::thread::current().id()) {
            Some((ids, _)) => std::mem::take(ids),
            None => return,
        }
    };
    READY.with(|ready| ready.borrow_mut().extend(ids));
}

/// Polls every task that was ready when it was called, once each. Tasks woken meanwhile wait
/// for the next call (and ask the scheduler for it). Answers whether anything was polled.
pub fn run_ready() -> bool {
    DRAIN_PENDING.with(|pending| pending.set(false));
    take_remote();
    let batch: VecDeque<u64> = READY.with(|ready| std::mem::take(&mut *ready.borrow_mut()));
    let mut ran = false;
    for id in batch {
        let task = TASKS.with(|tasks| tasks.borrow().get(&id).and_then(Weak::upgrade));
        if let Some(task) = task {
            ran = true;
            task.run();
        }
    }
    ran
}

/// Drains the ready queue until nothing is left to poll.
pub fn run_until_stalled() {
    while has_ready() {
        run_ready();
    }
}

enum State<T> {
    Running(LocalBoxFuture<T>),
    Polling,
    Done(T),
}

struct Inner<T> {
    id: u64,
    state: RefCell<State<T>>,
    waiters: RefCell<Vec<Waker>>,
    scope: Option<Rc<dyn Scope>>,
    /// Woken while being polled: poll again from the queue.
    woken_while_polling: Cell<bool>,
}

impl<T: 'static> Inner<T> {
    fn poll_once(self: &Rc<Self>) {
        let mut future = match std::mem::replace(&mut *self.state.borrow_mut(), State::Polling) {
            State::Running(future) => future,
            other => {
                // Done, or polled re-entrantly: put back and let the queue come round again.
                let polling = matches!(other, State::Polling);
                *self.state.borrow_mut() = other;
                if polling {
                    self.woken_while_polling.set(true);
                }
                return;
            }
        };
        let waker = Waker::from(Arc::new(TaskWaker {
            id: self.id,
            thread: std::thread::current().id(),
        }));
        let mut cx = Context::from_waker(&waker);
        match future.as_mut().poll(&mut cx) {
            Poll::Ready(value) => {
                *self.state.borrow_mut() = State::Done(value);
                TASKS.with(|tasks| tasks.borrow_mut().remove(&self.id));
                release(self.id);
                let waiters = std::mem::take(&mut *self.waiters.borrow_mut());
                for waiter in waiters {
                    waiter.wake();
                }
            }
            Poll::Pending => {
                *self.state.borrow_mut() = State::Running(future);
                if self.woken_while_polling.replace(false) {
                    schedule_local(self.id);
                }
            }
        }
    }
}

impl<T: 'static> Runnable for Inner<T> {
    fn run(self: Rc<Self>) {
        match self.scope.clone() {
            Some(scope) => scope.enter(&mut || self.poll_once()),
            None => self.poll_once(),
        }
    }
}

/// A spawned task: a handle that is itself a future of the task's output, and can be cloned
/// and awaited any number of times (as a JS promise can).
pub struct Task<T> {
    inner: Rc<Inner<T>>,
}

impl<T> Clone for Task<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T> fmt::Debug for Task<T> {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        f.debug_struct("Task").field("id", &self.inner.id).finish()
    }
}

impl<T: Clone + 'static> Task<T> {
    /// The output, if the task has finished.
    pub fn output(&self) -> Option<T> {
        match &*self.inner.state.borrow() {
            State::Done(value) => Some(value.clone()),
            _ => None,
        }
    }

    /// Whether the task has finished.
    pub fn is_done(&self) -> bool {
        matches!(&*self.inner.state.borrow(), State::Done(_))
    }

    /// Whether two handles are the same task.
    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<T: Clone + 'static> Future for Task<T> {
    type Output = T;

    fn poll(self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<T> {
        if let State::Done(value) = &*self.inner.state.borrow() {
            return Poll::Ready(value.clone());
        }
        self.inner.waiters.borrow_mut().push(cx.waker().clone());
        Poll::Pending
    }
}

/// Starts `future` now: polls it once before returning, so it runs as far as its first
/// pending await, as a JS `async` function runs to its first `await`.
pub fn spawn<T: 'static>(future: impl Future<Output = T> + 'static) -> Task<T> {
    spawn_in(None, future)
}

/// [`spawn`], with every poll after the first run inside `scope`. The first poll runs in
/// whatever scope the caller is in — which is the scope that was captured.
pub fn spawn_in<T: 'static>(
    scope: Option<Rc<dyn Scope>>,
    future: impl Future<Output = T> + 'static,
) -> Task<T> {
    let id = NEXT_ID.with(|next| {
        let id = next.get();
        next.set(id + 1);
        id
    });
    let inner = Rc::new(Inner {
        id,
        state: RefCell::new(State::Running(Box::pin(future))),
        waiters: RefCell::new(Vec::new()),
        scope,
        woken_while_polling: Cell::new(false),
    });
    let runnable: Rc<dyn Runnable> = inner.clone();
    TASKS.with(|tasks| tasks.borrow_mut().insert(id, Rc::downgrade(&runnable)));
    // Keep the task alive while it is pending: the map holds it weakly, the future is owned
    // by the task, and whoever awaits holds a handle. A task nobody holds that is still
    // pending would otherwise be dropped with its future — which a JS promise never is.
    KEEP.with(|keep| keep.borrow_mut().insert(id, runnable));
    inner.poll_once();
    Task { inner }
}

thread_local! {
    /// Pending tasks, owned until they finish.
    static KEEP: RefCell<HashMap<u64, Rc<dyn Runnable>>> = RefCell::new(HashMap::new());
}

/// Called by a finished task: it no longer needs keeping.
fn release(id: u64) {
    let _released = KEEP.with(|keep| keep.borrow_mut().remove(&id));
}

impl<T> Drop for Inner<T> {
    fn drop(&mut self) {
        // Also runs while the thread's locals are torn down (a pending task kept by `KEEP`),
        // when `TASKS` may already be gone.
        let _ = TASKS.try_with(|tasks| {
            if let Ok(mut tasks) = tasks.try_borrow_mut() {
                tasks.remove(&self.id);
            }
        });
    }
}

/// A future that is pending once and then ready: one hop through the ready queue, what
/// `await Promise.resolve()` is.
pub fn yield_now() -> YieldNow {
    YieldNow { yielded: false }
}

/// See [`yield_now`].
pub struct YieldNow {
    yielded: bool,
}

impl Future for YieldNow {
    type Output = ();

    fn poll(mut self: Pin<&mut Self>, cx: &mut Context<'_>) -> Poll<()> {
        if self.yielded {
            return Poll::Ready(());
        }
        self.yielded = true;
        cx.waker().wake_by_ref();
        Poll::Pending
    }
}

/// A one-shot settlement any number of tasks can await: what a pending JS promise is to the
/// code awaiting it.
pub struct Deferred<T> {
    inner: Rc<DeferredInner<T>>,
}

struct DeferredInner<T> {
    value: RefCell<Option<T>>,
    waiters: RefCell<Vec<Waker>>,
}

impl<T> Clone for Deferred<T> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<T: Clone + 'static> Default for Deferred<T> {
    fn default() -> Self {
        Self::new()
    }
}

impl<T: Clone + 'static> Deferred<T> {
    pub fn new() -> Self {
        Self {
            inner: Rc::new(DeferredInner {
                value: RefCell::new(None),
                waiters: RefCell::new(Vec::new()),
            }),
        }
    }

    /// Settles it, the first time only; answers whether this call did.
    pub fn settle(&self, value: T) -> bool {
        if self.inner.value.borrow().is_some() {
            return false;
        }
        *self.inner.value.borrow_mut() = Some(value);
        let waiters = std::mem::take(&mut *self.inner.waiters.borrow_mut());
        for waiter in waiters {
            waiter.wake();
        }
        true
    }

    pub fn is_settled(&self) -> bool {
        self.inner.value.borrow().is_some()
    }

    /// A future of the settlement.
    pub fn wait(&self) -> impl Future<Output = T> + 'static {
        let inner = self.inner.clone();
        std::future::poll_fn(move |cx| {
            if let Some(value) = &*inner.value.borrow() {
                return Poll::Ready(value.clone());
            }
            inner.waiters.borrow_mut().push(cx.waker().clone());
            Poll::Pending
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn a_task_runs_its_synchronous_prefix_at_once() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        let task = spawn(async move {
            l.borrow_mut().push("prefix");
            yield_now().await;
            l.borrow_mut().push("after");
            7
        });
        assert_eq!(*log.borrow(), ["prefix"]);
        assert!(!task.is_done());
        run_until_stalled();
        assert_eq!(*log.borrow(), ["prefix", "after"]);
        assert_eq!(task.output(), Some(7));
    }

    #[test]
    fn a_drain_polls_only_what_was_ready_when_it_started() {
        let log = Rc::new(RefCell::new(Vec::new()));
        let l = log.clone();
        spawn(async move {
            yield_now().await;
            l.borrow_mut().push(1);
            yield_now().await;
            l.borrow_mut().push(2);
        });
        run_ready();
        assert_eq!(*log.borrow(), [1]);
        run_ready();
        assert_eq!(*log.borrow(), [1, 2]);
    }

    #[test]
    fn deferreds_wake_every_waiter() {
        let deferred = Deferred::<u32>::new();
        let a = spawn(deferred.wait());
        let b = spawn(deferred.wait());
        assert!(!a.is_done() && !b.is_done());
        deferred.settle(3);
        run_until_stalled();
        assert_eq!((a.output(), b.output()), (Some(3), Some(3)));
    }

    #[test]
    fn the_scheduler_is_asked_once_per_empty_queue() {
        let asked = Rc::new(Cell::new(0));
        let a = asked.clone();
        set_scheduler(Some(Rc::new(move || {
            a.set(a.get() + 1);
            true
        })));
        spawn(async {
            yield_now().await;
        });
        spawn(async {
            yield_now().await;
        });
        assert_eq!(asked.get(), 1);
        run_ready();
        assert_eq!(asked.get(), 1);
        set_scheduler(None);
    }
}
