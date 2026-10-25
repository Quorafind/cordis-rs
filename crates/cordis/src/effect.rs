//! Effects and disposers: `fiber.effect(execute, label)` and what it collects.
//!
//! An effect runs its body once, collects every disposer the body produces (a function it
//! returns, every value a generator yields, what a promise settles with), and answers a
//! disposer of its own. Disposing runs the collected disposers last-first; a disposer that
//! answers a thenable makes the rest wait for it, chained, as `task.then(dispose)` does.
//!
//! A disposer keeps its identity across the host boundary: the face of an [`Effect`] the host
//! hands back (a generator yielding `ctx.on(...)`'s answer) is recognised as that effect, so a
//! composite effect *owns* the inner one — it leaves the fiber's own list and is disposed in
//! the composite's order instead. DSH's agent factory depends on exactly that.

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use crate::host::{EffectValue, ErrorKind, Host};
use crate::root::RootInner;
use crate::task::{self, Task, yield_now};

/// The completion of an asynchronous disposal or execution.
pub type Pending<H> = Task<Result<(), <H as Host>::Error>>;

/// A native disposer.
pub type NativeDisposer<H> = Rc<dyn Fn() -> Result<Option<Pending<H>>, <H as Host>::Error>>;

/// Something that undoes something.
pub enum Disposer<H: Host> {
    /// An effect's own disposer.
    Effect(Effect<H>),
    /// A host function.
    Value(H::Value),
    /// A Rust closure.
    Native(NativeDisposer<H>),
}

impl<H: Host> Clone for Disposer<H> {
    fn clone(&self) -> Self {
        match self {
            Self::Effect(effect) => Self::Effect(effect.clone()),
            Self::Value(value) => Self::Value(value.clone()),
            Self::Native(f) => Self::Native(f.clone()),
        }
    }
}

impl<H: Host> Disposer<H> {
    /// Calls it. `Ok(Some(task))` when it answered a thenable.
    pub fn call(&self, host: &H) -> Result<Option<Pending<H>>, H::Error> {
        match self {
            Self::Effect(effect) => effect.dispose(),
            Self::Native(f) => f(),
            Self::Value(f) => {
                let result = host.call(f, &host.undefined(), &[])?;
                if !host.is_thenable(&result) {
                    return Ok(None);
                }
                let settled = host.await_value(result);
                Ok(Some(task::spawn_in(host.scope(), async move {
                    settled.await.map(|_| ())
                })))
            }
        }
    }

    /// Whether two disposers are the same one.
    pub fn same(&self, other: &Self, host: &H) -> bool {
        match (self, other) {
            (Self::Effect(a), Self::Effect(b)) => Rc::ptr_eq(&a.inner, &b.inner),
            (Self::Value(a), Self::Value(b)) => host.same(a, b),
            (Self::Native(a), Self::Native(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// The effect it is, if it is one.
    pub fn as_effect(&self) -> Option<&Effect<H>> {
        match self {
            Self::Effect(effect) => Some(effect),
            _ => None,
        }
    }
}

/// An effect: its label, its children, and what it will undo.
pub struct Effect<H: Host> {
    pub(crate) inner: Rc<EffectInner<H>>,
}

impl<H: Host> Clone for Effect<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct EffectInner<H: Host> {
    pub(crate) label: String,
    pub(crate) children: RefCell<Vec<Effect<H>>>,
    /// `runner.epoch`: true until disposed.
    pub(crate) epoch: Cell<bool>,
    pub(crate) disposables: RefCell<Vec<Disposer<H>>>,
    /// The execution, when it was asynchronous.
    pub(crate) task: RefCell<Option<Pending<H>>>,
    pub(crate) root: Weak<RootInner<H>>,
    /// The host's face, made once so its identity is stable; let go when disposed.
    pub(crate) face: RefCell<Option<H::Value>>,
}

impl<H: Host> Effect<H> {
    pub(crate) fn new(label: String, root: Weak<RootInner<H>>) -> Self {
        Self {
            inner: Rc::new(EffectInner {
                label,
                children: RefCell::new(Vec::new()),
                epoch: Cell::new(true),
                disposables: RefCell::new(Vec::new()),
                task: RefCell::new(None),
                root,
                face: RefCell::new(None),
            }),
        }
    }

    pub fn label(&self) -> &str {
        &self.inner.label
    }

    /// The effects this one collected, in collection order: `meta.children`.
    pub fn children(&self) -> Vec<Effect<H>> {
        self.inner.children.borrow().clone()
    }

    /// Whether it has not been disposed yet.
    pub fn is_active(&self) -> bool {
        self.inner.epoch.get()
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// The execution, when it is (or was) asynchronous.
    pub fn task(&self) -> Option<Pending<H>> {
        self.inner.task.borrow().clone()
    }

    /// The face the host made for it, cached.
    pub fn face(&self) -> Option<H::Value> {
        self.inner.face.borrow().clone()
    }

    pub fn set_face(&self, face: Option<H::Value>) {
        *self.inner.face.borrow_mut() = face;
    }

    fn root(&self) -> Option<Rc<RootInner<H>>> {
        self.inner.root.upgrade()
    }

    /// Runs the collected disposers, last-first; a thenable answer chains the rest after it.
    /// A disposer that throws stops the run and the error is answered — the rest are dropped,
    /// as upstream's loop drops them.
    pub(crate) fn run_disposables(&self) -> Result<Option<Pending<H>>, H::Error> {
        let Some(root) = self.root() else {
            self.inner.disposables.borrow_mut().clear();
            return Ok(None);
        };
        let list: Vec<Disposer<H>> = {
            let mut disposables = self.inner.disposables.borrow_mut();
            disposables.drain(..).rev().collect()
        };
        let mut pending: Option<Pending<H>> = None;
        for disposer in list {
            match pending.take() {
                Some(before) => {
                    let root = root.clone();
                    pending = Some(task::spawn_in(root.host.scope(), async move {
                        before.await?;
                        match disposer.call(&root.host)? {
                            Some(after) => after.await,
                            None => Ok(()),
                        }
                    }));
                }
                None => pending = disposer.call(&root.host)?,
            }
        }
        Ok(pending)
    }

    /// The disposer the effect answers: once only; after an asynchronous execution, waits for
    /// it first.
    pub fn dispose(&self) -> Result<Option<Pending<H>>, H::Error> {
        if !self.inner.epoch.get() {
            return Ok(None);
        }
        self.inner.epoch.set(false);
        self.inner.face.borrow_mut().take();
        let execution = self.inner.task.borrow().clone();
        let result = match execution {
            Some(execution) => {
                let effect = self.clone();
                let scope = self.root().and_then(|root| root.host.scope());
                Ok(Some(task::spawn_in(scope, async move {
                    execution.await?;
                    match effect.run_disposables()? {
                        Some(after) => after.await,
                        None => Ok(()),
                    }
                })))
            }
            None => self.run_disposables(),
        };
        result
    }

    /// `disposeAsync`, what `await effect` resolves to: disposes without waiting for the
    /// execution.
    pub fn dispose_now(&self) -> Result<Option<Pending<H>>, H::Error> {
        if !self.inner.epoch.get() {
            return Ok(None);
        }
        self.inner.epoch.set(false);
        self.inner.face.borrow_mut().take();
        self.run_disposables()
    }
}

/// What a body is run with, and where what it produces goes: a fiber's plugin runner, or an
/// effect's.
pub(crate) struct Runner<H: Host> {
    pub(crate) execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>>,
    pub(crate) collect: Rc<dyn Fn(Disposer<H>)>,
    /// The runner's epoch as a comparable token; an async iterator stops when it changes.
    pub(crate) epoch: Rc<dyn Fn() -> Option<String>>,
}

impl<H: Host> Clone for Runner<H> {
    fn clone(&self) -> Self {
        Self {
            execute: self.execute.clone(),
            collect: self.collect.clone(),
            epoch: self.epoch.clone(),
        }
    }
}

fn safe_collect<H: Host>(
    host: &H,
    collect: &Rc<dyn Fn(Disposer<H>)>,
    value: &H::Value,
) -> Result<(), H::Error> {
    if let Some(disposer) = host.as_disposer(value) {
        collect(disposer);
        Ok(())
    } else if host.is_nullish(value) {
        Ok(())
    } else {
        Err(host.error(ErrorKind::TypeError, "Invalid effect"))
    }
}

/// `Fiber._execute`: runs the body and collects what it produced. A synchronous body is done
/// when this returns; an asynchronous one answers the task collecting the rest.
pub(crate) fn execute<H: Host>(
    root: &Rc<RootInner<H>>,
    runner: &Runner<H>,
) -> Result<Option<Pending<H>>, H::Error> {
    let host = &root.host;
    let old_epoch = (runner.epoch)();
    let value = (runner.execute)().map_err(|e| host.compose_error(e))?;
    match value {
        EffectValue::Nullish => Ok(None),
        EffectValue::Invalid => Err(host.error(ErrorKind::TypeError, "Invalid effect")),
        EffectValue::Dispose(disposer) => {
            (runner.collect)(disposer);
            Ok(None)
        }
        EffectValue::Thenable(settled) => {
            let root = root.clone();
            let collect = runner.collect.clone();
            Ok(Some(task::spawn_in(root.host.scope(), async move {
                let host = &root.host;
                let value = settled.await.map_err(|e| host.compose_error(e))?;
                safe_collect(host, &collect, &value).map_err(|e| host.compose_error(e))
            })))
        }
        EffectValue::Iter(mut iter) => loop {
            let (value, done) = iter.next().map_err(|e| host.compose_error(e))?;
            safe_collect(host, &runner.collect, &value)?;
            if done {
                return Ok(None);
            }
        },
        EffectValue::AsyncIter(mut iter) => {
            let root = root.clone();
            let runner = runner.clone();
            Ok(Some(task::spawn_in(root.host.scope(), async move {
                let host = &root.host;
                yield_now().await;
                loop {
                    if (runner.epoch)() != old_epoch {
                        return Ok(());
                    }
                    let (value, done) = iter.next().await.map_err(|e| host.compose_error(e))?;
                    safe_collect(host, &runner.collect, &value)
                        .map_err(|e| host.compose_error(e))?;
                    if done {
                        return Ok(());
                    }
                }
            })))
        }
    }
}
