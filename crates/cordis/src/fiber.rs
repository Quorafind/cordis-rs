//! Fibers: one plugin instance on one context, and its lifecycle.
//!
//! A fiber is `PENDING` until every service it injects is available, then `LOADING` while its
//! body runs, `ACTIVE` once it has, `UNLOADING` while its effects are undone (when a service it
//! injects goes away or changes provider, or it is disposed), `FAILED` when its body threw, and
//! `DISPOSED` for good. Which providers it was loaded against is its *epoch*: a change of epoch
//! unloads and reloads it. A load or unload in flight is its *inertia*; epoch changes during
//! one wait for it, and are reconciled when it ends — the inertia lock.

use std::cell::{Cell, RefCell};
use std::future::Future;
use std::rc::{Rc, Weak};

use indexmap::IndexMap;

use crate::effect::{self, Disposer, Effect, Pending, Runner};
use crate::host::{EffectValue, ErrorKind, Host};
use crate::list::DisposableList;
use crate::root::{Callback, Impl, Root, RootInner, Runtime};
use crate::task::{self, Task, yield_now};

/// A fiber's state, numbered as `FiberState` is.
#[derive(Clone, Copy, Debug, PartialEq, Eq, Hash)]
#[repr(u8)]
pub enum FiberState {
    Pending = 0,
    Loading = 1,
    Active = 2,
    Failed = 3,
    Disposed = 4,
    Unloading = 5,
}

impl FiberState {
    pub fn as_number(self) -> f64 {
        self as u8 as f64
    }

    pub fn name(self) -> &'static str {
        match self {
            Self::Pending => "PENDING",
            Self::Loading => "LOADING",
            Self::Active => "ACTIVE",
            Self::Failed => "FAILED",
            Self::Disposed => "DISPOSED",
            Self::Unloading => "UNLOADING",
        }
    }
}

/// Which providers a fiber is loaded against: `":uid:uid…"` of its injected services, or
/// inactive when one is missing.
#[derive(Clone, Debug, PartialEq, Eq)]
pub enum Epoch {
    Inactive,
    Active(String),
}

impl Epoch {
    fn token(&self) -> Option<String> {
        match self {
            Self::Inactive => Some("\0inactive".into()),
            Self::Active(text) => Some(text.clone()),
        }
    }
}

/// A plugin instance on a context (or the root's own).
pub struct Fiber<H: Host> {
    pub(crate) inner: Rc<FiberInner<H>>,
}

impl<H: Host> Clone for Fiber<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<H: Host> PartialEq for Fiber<H> {
    fn eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl<H: Host> Eq for Fiber<H> {}

pub(crate) struct FiberInner<H: Host> {
    pub(crate) root: Weak<RootInner<H>>,
    pub(crate) parent: Option<Weak<FiberInner<H>>>,
    /// `fiber.parent`: the context it was made on.
    pub(crate) parent_ctx: RefCell<Option<H::Ctx>>,
    pub(crate) runtime: Option<Runtime<H>>,
    pub(crate) uid: Cell<Option<u32>>,
    pub(crate) state: Cell<FiberState>,
    /// Its own context; let go once it is disposed (it holds the fiber's face, which holds
    /// the fiber).
    pub(crate) ctx: RefCell<Option<H::Ctx>>,
    /// What it injects, in declaration order, with each one's config.
    pub(crate) inject: Vec<(String, Option<H::Value>)>,
    pub(crate) config: RefCell<Option<H::Value>>,
    pub(crate) error: RefCell<Option<H::Error>>,
    pub(crate) epoch: RefCell<Epoch>,
    pub(crate) inertia: RefCell<Option<Task<()>>>,
    /// The services it was loaded with (and those it provides); `None` while unloaded.
    pub(crate) store: RefCell<Option<IndexMap<String, Rc<Impl<H>>>>>,
    /// `_store`: the impls of its injected services that are present and pass their checks.
    pub(crate) checked: RefCell<IndexMap<String, Rc<Impl<H>>>>,
    pub(crate) disposables: DisposableList<Disposer<H>>,
    /// Its own `internal/update` listeners.
    pub(crate) update_hooks: DisposableList<Callback<H>>,
    /// The effect, on its parent, that disposes it.
    pub(crate) dispose: RefCell<Option<Effect<H>>>,
    pub(crate) face: RefCell<Option<H::Value>>,
}

impl<H: Host> FiberInner<H> {
    pub(crate) fn blank(
        root: Weak<RootInner<H>>,
        parent: Option<Weak<FiberInner<H>>>,
        inject: Vec<(String, Option<H::Value>)>,
        runtime: Option<Runtime<H>>,
    ) -> Self {
        Self {
            root,
            parent,
            parent_ctx: RefCell::new(None),
            runtime,
            uid: Cell::new(None),
            state: Cell::new(FiberState::Pending),
            ctx: RefCell::new(None),
            inject,
            config: RefCell::new(None),
            error: RefCell::new(None),
            epoch: RefCell::new(Epoch::Inactive),
            inertia: RefCell::new(None),
            store: RefCell::new(None),
            checked: RefCell::new(IndexMap::new()),
            disposables: DisposableList::new(),
            update_hooks: DisposableList::new(),
            dispose: RefCell::new(None),
            face: RefCell::new(None),
        }
    }
}

/// What the registry is told about a plugin when it is first used.
pub struct PluginSpec<H: Host> {
    pub key: H::PluginKey,
    pub callback: H::Value,
    pub name: Option<String>,
    pub schema: Option<H::Value>,
    pub inject: Vec<(String, Option<H::Value>)>,
}

impl<H: Host> Fiber<H> {
    pub(crate) fn root_inner(&self) -> Option<Rc<RootInner<H>>> {
        self.inner.root.upgrade()
    }

    /// The root of its tree.
    pub fn root(&self) -> Option<Root<H>> {
        self.root_inner().map(Root::from_inner)
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    pub fn downgrade(&self) -> WeakFiber<H> {
        WeakFiber {
            inner: Rc::downgrade(&self.inner),
        }
    }

    /// `null` once disposed.
    pub fn uid(&self) -> Option<u32> {
        self.inner.uid.get()
    }

    pub fn state(&self) -> FiberState {
        self.inner.state.get()
    }

    pub fn is_root(&self) -> bool {
        self.inner.runtime.is_none()
    }

    pub fn runtime(&self) -> Option<Runtime<H>> {
        self.inner.runtime.clone()
    }

    pub fn ctx(&self) -> Option<H::Ctx> {
        self.inner.ctx.borrow().clone()
    }

    pub fn parent_ctx(&self) -> Option<H::Ctx> {
        self.inner.parent_ctx.borrow().clone()
    }

    pub fn parent(&self) -> Option<Fiber<H>> {
        self.inner
            .parent
            .as_ref()
            .and_then(Weak::upgrade)
            .map(|inner| Fiber { inner })
    }

    pub fn config(&self) -> Option<H::Value> {
        self.inner.config.borrow().clone()
    }

    pub fn set_config(&self, config: H::Value) {
        *self.inner.config.borrow_mut() = Some(config);
    }

    pub fn error(&self) -> Option<H::Error> {
        self.inner.error.borrow().clone()
    }

    pub fn set_error(&self, error: Option<H::Error>) {
        *self.inner.error.borrow_mut() = error;
    }

    pub fn inject(&self) -> &[(String, Option<H::Value>)] {
        &self.inner.inject
    }

    pub fn injects(&self, name: &str) -> bool {
        self.inner.inject.iter().any(|(n, _)| n == name)
    }

    pub fn epoch(&self) -> Epoch {
        self.inner.epoch.borrow().clone()
    }

    /// The load or unload in flight.
    pub fn inertia(&self) -> Option<Task<()>> {
        self.inner.inertia.borrow().clone()
    }

    /// The effect that disposes it (on its parent); none for the root.
    pub fn dispose_effect(&self) -> Option<Effect<H>> {
        self.inner.dispose.borrow().clone()
    }

    /// `fiber.store`: the services it can reach without walking up, while loaded.
    pub fn store(&self) -> Option<Vec<(String, Rc<Impl<H>>)>> {
        self.inner
            .store
            .borrow()
            .as_ref()
            .map(|store| store.iter().map(|(k, v)| (k.clone(), v.clone())).collect())
    }

    /// One service of `fiber.store`.
    pub fn stored(&self, name: &str) -> Option<Rc<Impl<H>>> {
        self.inner
            .store
            .borrow()
            .as_ref()
            .and_then(|store| store.get(name).cloned())
    }

    /// `_store`.
    pub fn checked(&self) -> Vec<(String, Rc<Impl<H>>)> {
        self.inner
            .checked
            .borrow()
            .iter()
            .map(|(k, v)| (k.clone(), v.clone()))
            .collect()
    }

    /// The face the host made for it, cached while it is alive.
    pub fn face(&self) -> Option<H::Value> {
        self.inner.face.borrow().clone()
    }

    pub fn set_face(&self, face: Option<H::Value>) {
        *self.inner.face.borrow_mut() = face;
    }

    /// Its own `internal/update` listeners.
    pub fn update_hooks(&self) -> &DisposableList<Callback<H>> {
        &self.inner.update_hooks
    }

    /// `fiber.name`: the nearest runtime name up the tree, or `"root"`.
    pub fn name(&self) -> String {
        let mut fiber = Some(self.clone());
        while let Some(current) = fiber {
            if let Some(name) = current.runtime().and_then(|r| r.name().map(str::to_owned)) {
                return name;
            }
            if current.is_root() {
                break;
            }
            fiber = current.parent();
        }
        "root".into()
    }

    pub(crate) fn report(&self, error: &H::Error) {
        if let Some(root) = self.root() {
            root.report(self.ctx().as_ref(), error);
        }
    }

    /// `assertActive`: a disposed fiber makes nothing new.
    pub fn assert_active(&self) -> Result<(), H::Error> {
        if self.uid().is_some() {
            return Ok(());
        }
        let root = self.root_inner();
        Err(match root {
            Some(root) => root.host.error(
                ErrorKind::Cordis("INACTIVE_EFFECT"),
                "cannot create effect on inactive context",
            ),
            None => panic!("a fiber outlived its root"),
        })
    }

    /// Its effects, as `fiber.getEffects()` lists them.
    pub fn effects(&self) -> Vec<Effect<H>> {
        self.inner
            .disposables
            .values()
            .into_iter()
            .filter_map(|d| d.as_effect().cloned())
            .collect()
    }

    /// The disposers it holds, in order.
    pub fn disposables(&self) -> Vec<Disposer<H>> {
        self.inner.disposables.values()
    }

    /// Clears its disposables without running them: what `new Context()` does to the root's
    /// built-in registrations, which are never undone.
    pub fn forget_disposables(&self) {
        self.inner.disposables.clear();
    }

    // ─── Effects ─────────────────────────────────────────────────────────────────────────

    /// `fiber.effect(execute, label)`.
    pub fn effect(
        &self,
        execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>>,
        label: impl Into<String>,
    ) -> Result<Effect<H>, H::Error> {
        self.assert_active()?;
        let root = self.root_inner().expect("a live fiber has its root");
        let effect = Effect::new(label.into(), Rc::downgrade(&root));
        let collect: Rc<dyn Fn(Disposer<H>)> = {
            let effect = effect.clone();
            let fiber = Rc::downgrade(&self.inner);
            let root = Rc::downgrade(&root);
            Rc::new(move |disposer: Disposer<H>| {
                effect.inner.disposables.borrow_mut().push(disposer.clone());
                if let (Some(fiber), Some(root)) = (fiber.upgrade(), root.upgrade()) {
                    fiber
                        .disposables
                        .remove_where(|d| d.same(&disposer, &root.host));
                }
                if let Disposer::Effect(child) = &disposer {
                    effect.inner.children.borrow_mut().push(child.clone());
                }
            })
        };
        let epoch: Rc<dyn Fn() -> Option<String>> = {
            let effect = effect.clone();
            Rc::new(move || effect.inner.epoch.get().then(String::new))
        };
        let runner = Runner {
            execute,
            collect,
            epoch,
        };
        let pending = match effect::execute(&root, &runner) {
            Ok(pending) => pending,
            Err(error) => {
                let _ = effect.run_disposables();
                return Err(error);
            }
        };
        if let Some(pending) = &pending {
            *effect.inner.task.borrow_mut() = Some(pending.clone());
            // `task.catch(dispose).catch(error => ctx.logger.error(error))`: a failed
            // execution undoes what it had collected; only a failing undo is reported.
            let pending = pending.clone();
            let effect = effect.clone();
            let fiber = self.downgrade();
            task::spawn_in(root.host.scope(), async move {
                if pending.await.is_ok() {
                    return;
                }
                let failure = match effect.run_disposables() {
                    Ok(Some(after)) => after.await.err(),
                    Ok(None) => None,
                    Err(error) => Some(error),
                };
                if let (Some(error), Some(fiber)) = (failure, fiber.upgrade()) {
                    fiber.report(&error);
                }
            });
        }
        let sn = self
            .inner
            .disposables
            .push(Disposer::Effect(effect.clone()));
        let fiber = Rc::downgrade(&self.inner);
        effect
            .inner
            .disposables
            .borrow_mut()
            .push(Disposer::Native(Rc::new(move || {
                if let Some(fiber) = fiber.upgrade() {
                    fiber.disposables.remove(sn);
                }
                Ok(None)
            })));
        Ok(effect)
    }

    // ─── The lifecycle ───────────────────────────────────────────────────────────────────

    /// `_getState`.
    fn computed_state(&self) -> FiberState {
        if self.uid().is_none() {
            FiberState::Disposed
        } else if self.inner.error.borrow().is_some() {
            FiberState::Failed
        } else if *self.inner.epoch.borrow() != Epoch::Inactive {
            FiberState::Active
        } else {
            FiberState::Pending
        }
    }

    /// `_updateState`: sets the state `transition` answers (or the computed one), tells
    /// `internal/status`, and when the fiber enters or leaves `ACTIVE`, re-notifies the
    /// services it provides.
    fn update_state(
        &self,
        transition: impl FnOnce() -> Option<FiberState>,
    ) -> Result<(), H::Error> {
        let old = self.inner.state.get();
        let new = transition().unwrap_or_else(|| self.computed_state());
        self.inner.state.set(new);
        if old == new {
            return Ok(());
        }
        let Some(root) = self.root() else {
            return Ok(());
        };
        let host = root.host();
        root.emit(
            None,
            "internal/status",
            &[host.fiber_value(self), host.number(old.as_number())],
        )?;
        if old != FiberState::Active && new != FiberState::Active {
            return Ok(());
        }
        let provided: Vec<String> = root
            .impls()
            .into_iter()
            .filter(|(_, imp)| imp.fiber == *self)
            .map(|(_, imp)| imp.name.clone())
            .collect();
        if let Some(ctx) = self.ctx() {
            for name in provided {
                root.notify(&ctx, &[name])?;
            }
        }
        Ok(())
    }

    /// `_checkImpl`: whether the service `name` is there for this fiber, and passes its check.
    pub(crate) fn check_impl(&self, name: &str) {
        let (Some(root), Some(ctx)) = (self.root(), self.ctx()) else {
            return;
        };
        let Some(imp) = root.get_impl(&ctx, name, true) else {
            self.inner.checked.borrow_mut().shift_remove(name);
            return;
        };
        if let Some(check) = &imp.check {
            let value = imp.value.borrow().clone();
            match root.host().check_service(check, &ctx, &value) {
                Ok(true) => {}
                Ok(false) => {
                    self.inner.checked.borrow_mut().shift_remove(name);
                    return;
                }
                Err(error) => {
                    imp.fiber.report(&error);
                    self.inner.checked.borrow_mut().shift_remove(name);
                    return;
                }
            }
        }
        self.inner.checked.borrow_mut().insert(name.to_owned(), imp);
    }

    /// `_refresh`: the epoch its injected services make now.
    pub(crate) fn refresh(&self) -> Result<(), H::Error> {
        let mut epoch = String::new();
        for (name, _) in &self.inner.inject {
            let provider = self
                .inner
                .checked
                .borrow()
                .get(name)
                .map(|imp| imp.fiber.uid());
            match provider {
                None => return self.set_epoch(Epoch::Inactive),
                Some(uid) => {
                    epoch.push(':');
                    match uid {
                        Some(uid) => epoch.push_str(&uid.to_string()),
                        None => epoch.push_str("null"),
                    }
                }
            }
        }
        self.set_epoch(Epoch::Active(epoch))
    }

    /// `_setEpoch`: a new epoch starts a load or an unload, unless one is in flight — then it
    /// waits, and the one in flight reconciles when it ends.
    pub(crate) fn set_epoch(&self, epoch: Epoch) -> Result<(), H::Error> {
        let old = self.inner.epoch.borrow().clone();
        if epoch == old {
            return Ok(());
        }
        *self.inner.epoch.borrow_mut() = epoch.clone();
        if self.inner.inertia.borrow().is_some() {
            return Ok(());
        }
        self.update_state(|| {
            if epoch != Epoch::Inactive && old == Epoch::Inactive {
                let task = self.reload();
                *self.inner.inertia.borrow_mut() = Some(task);
                Some(FiberState::Loading)
            } else {
                let task = self.unload();
                *self.inner.inertia.borrow_mut() = Some(task);
                Some(FiberState::Unloading)
            }
        })
    }

    fn plugin_runner(&self) -> Runner<H> {
        let fiber = self.downgrade();
        let execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>> = {
            let fiber = fiber.clone();
            Rc::new(move || {
                let Some(fiber) = fiber.upgrade() else {
                    return Ok(EffectValue::Nullish);
                };
                let (Some(root), Some(runtime)) = (fiber.root(), fiber.runtime()) else {
                    return Ok(EffectValue::Nullish);
                };
                let Some(ctx) = fiber.ctx() else {
                    return Ok(EffectValue::Nullish);
                };
                let host = root.host();
                let config = fiber.config().unwrap_or_else(|| host.undefined());
                let value = host.execute_plugin(runtime.callback(), &ctx, &config)?;
                Ok(host.classify(value))
            })
        };
        let collect: Rc<dyn Fn(Disposer<H>)> = {
            let fiber = fiber.clone();
            Rc::new(move |disposer| {
                if let Some(fiber) = fiber.upgrade() {
                    fiber.inner.disposables.push(disposer);
                }
            })
        };
        let epoch: Rc<dyn Fn() -> Option<String>> = Rc::new(move || {
            fiber
                .upgrade()
                .and_then(|fiber| fiber.inner.epoch.borrow().token())
        });
        Runner {
            execute,
            collect,
            epoch,
        }
    }

    /// `_reload`: snapshots the store now, then — a microtask later — runs the body.
    fn reload(&self) -> Task<()> {
        let snapshot = self.inner.checked.borrow().clone();
        *self.inner.store.borrow_mut() = Some(snapshot);
        let old = self.inner.epoch.borrow().clone();
        let fiber = self.clone();
        let scope = self.root_inner().and_then(|root| root.host.scope());
        task::spawn_in(scope, async move {
            yield_now().await;
            let outcome = if fiber.is_root() {
                yield_now().await;
                Ok(())
            } else {
                match fiber.root_inner() {
                    None => Ok(()),
                    Some(root) => match effect::execute(&root, &fiber.plugin_runner()) {
                        Ok(Some(pending)) => pending.await,
                        Ok(None) => {
                            yield_now().await;
                            Ok(())
                        }
                        Err(error) => Err(error),
                    },
                }
            };
            if let Err(error) = outcome {
                fiber.report(&error);
                *fiber.inner.error.borrow_mut() = Some(error);
                *fiber.inner.epoch.borrow_mut() = Epoch::Inactive;
            }
            let result = fiber.update_state(|| {
                if *fiber.inner.epoch.borrow() == old {
                    *fiber.inner.inertia.borrow_mut() = None;
                    None
                } else {
                    let task = fiber.unload();
                    *fiber.inner.inertia.borrow_mut() = Some(task);
                    Some(FiberState::Unloading)
                }
            });
            if let Err(error) = result {
                fiber.report(&error);
            }
        })
    }

    /// `_unload`: every disposer, last-first, each a microtask later; then the store goes, and
    /// if the epoch came back meanwhile, a reload.
    fn unload(&self) -> Task<()> {
        let disposers = self.inner.disposables.clear();
        let scope = self.root_inner().and_then(|root| root.host.scope());
        let undo: Vec<Task<()>> = disposers
            .into_iter()
            .map(|disposer| {
                let fiber = self.clone();
                task::spawn_in(scope.clone(), async move {
                    yield_now().await;
                    let Some(root) = fiber.root_inner() else {
                        return;
                    };
                    let outcome = match disposer.call(&root.host) {
                        Ok(Some(pending)) => pending.await,
                        Ok(None) => Ok(()),
                        Err(error) => Err(error),
                    };
                    if let Err(error) = outcome {
                        let error = root.host.compose_error(error);
                        fiber.report(&error);
                    }
                })
            })
            .collect();
        let fiber = self.clone();
        task::spawn_in(scope, async move {
            if undo.is_empty() {
                yield_now().await;
            }
            for task in undo {
                task.await;
            }
            *fiber.inner.store.borrow_mut() = None;
            let result = fiber.update_state(|| {
                if *fiber.inner.epoch.borrow() == Epoch::Inactive {
                    *fiber.inner.inertia.borrow_mut() = None;
                    None
                } else {
                    let task = fiber.reload();
                    *fiber.inner.inertia.borrow_mut() = Some(task);
                    Some(FiberState::Loading)
                }
            });
            if let Err(error) = result {
                fiber.report(&error);
            }
        })
    }

    /// `fiber.await()`: until no load or unload is in flight; then its error, if it failed.
    pub fn wait(&self) -> impl Future<Output = Result<(), H::Error>> + 'static {
        let fiber = self.clone();
        async move {
            loop {
                let inertia = fiber.inner.inertia.borrow().clone();
                match inertia {
                    Some(task) => task.await,
                    None => break,
                }
            }
            match fiber.error() {
                Some(error) => Err(error),
                None => Ok(()),
            }
        }
    }

    /// `fiber.restart()`: unloads and reloads it with the providers it has now.
    pub fn restart(&self) -> Task<Result<(), H::Error>> {
        let fiber = self.clone();
        let scope = self.root_inner().and_then(|root| root.host.scope());
        task::spawn_in(scope, async move {
            fiber.assert_active()?;
            fiber.set_epoch(Epoch::Inactive)?;
            fiber.refresh()?;
            fiber.wait().await
        })
    }

    /// `fiber.update(config, noSave)`: validates the config, then runs `internal/update` —
    /// the fiber's own listeners first — whose last step stores the config and restarts.
    pub fn update(&self, config: H::Value, no_save: bool) -> Result<(), H::Error> {
        self.assert_active()?;
        let root = self.root().expect("a live fiber has its root");
        let host = root.host();
        let schema = self.runtime().and_then(|r| r.schema().cloned());
        let config = host.resolve_config(schema.as_ref(), config)?;
        let fiber = self.clone();
        let applied = config.clone();
        let next: Callback<H> = Callback::Native(Rc::new(move |_, _| {
            fiber.set_config(applied.clone());
            fiber.set_error(None);
            let restart = fiber.restart();
            let root = fiber.root().expect("a live fiber has its root");
            Ok(root.task_value(restart))
        }));
        root.waterfall(
            Some(host.fiber_value(self)),
            "internal/update",
            vec![config, host.boolean(no_save)],
            next,
        )?;
        Ok(())
    }

    /// Lets go of what ties it to the host once it is disposed for good.
    fn release(&self) {
        *self.inner.ctx.borrow_mut() = None;
        *self.inner.face.borrow_mut() = None;
        self.inner.checked.borrow_mut().clear();
        self.inner.update_hooks.clear();
    }

    /// Makes a plugin fiber on `parent_ctx`, as `new Fiber(parent, config, inject, runtime)`
    /// does: its context, `internal/plugin`, its injections checked, and the effect on the
    /// parent that starts it and will dispose it.
    pub(crate) fn plugin(
        root: &Root<H>,
        parent_ctx: &H::Ctx,
        parent: &Fiber<H>,
        runtime: Runtime<H>,
        inject: Vec<(String, Option<H::Value>)>,
        config: H::Value,
    ) -> Result<Fiber<H>, H::Error> {
        let host = root.host();
        let fiber = Fiber {
            inner: Rc::new(FiberInner::blank(
                root.downgrade(),
                Some(Rc::downgrade(&parent.inner)),
                inject,
                Some(runtime.clone()),
            )),
        };
        fiber.inner.uid.set(Some(root.next_uid()));
        *fiber.inner.parent_ctx.borrow_mut() = Some(parent_ctx.clone());
        let configured: Vec<(String, H::Value)> = fiber
            .inner
            .inject
            .iter()
            .filter_map(|(name, config)| {
                config
                    .as_ref()
                    .filter(|config| !host.is_nullish(config))
                    .map(|config| (name.clone(), config.clone()))
            })
            .collect();
        let ctx = host.derive_fiber_ctx(
            parent_ctx,
            &fiber,
            &configured,
            !fiber.inner.inject.is_empty(),
        );
        *fiber.inner.ctx.borrow_mut() = Some(ctx);
        root.emit(None, "internal/plugin", &[host.fiber_value(&fiber)])?;
        let names: Vec<String> = fiber.inner.inject.iter().map(|(n, _)| n.clone()).collect();
        for name in &names {
            fiber.check_impl(name);
        }
        let execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>> = {
            let fiber = fiber.clone();
            let runtime = runtime.clone();
            let config = RefCell::new(Some(config));
            Rc::new(move || {
                let root = fiber.root().expect("a live fiber has its root");
                let host = root.host();
                let sn = runtime.inner.fibers.push(fiber.clone());
                let config = config
                    .borrow_mut()
                    .take()
                    .unwrap_or_else(|| host.undefined());
                let started = host
                    .resolve_config(runtime.schema(), config)
                    .and_then(|config| {
                        fiber.set_config(config);
                        fiber.refresh()
                    });
                if let Err(error) = started {
                    fiber.report(&error);
                    fiber.set_error(Some(error));
                }
                let fiber = fiber.clone();
                let runtime = runtime.clone();
                Ok(EffectValue::Dispose(Disposer::Native(Rc::new(move || {
                    let fiber = fiber.clone();
                    let runtime = runtime.clone();
                    let scope = fiber.root_inner().and_then(|root| root.host.scope());
                    Ok(Some(task::spawn_in(scope, async move {
                        fiber.inner.uid.set(None);
                        let Some(root) = fiber.root() else {
                            return Ok(());
                        };
                        let host = root.host();
                        root.emit(None, "internal/plugin", &[host.fiber_value(&fiber)])?;
                        if root.runtime(runtime.key()).is_some() {
                            runtime.inner.fibers.remove(sn);
                            if runtime.inner.fibers.is_empty() {
                                root.delete_runtime(runtime.key());
                            }
                        }
                        fiber.set_epoch(Epoch::Inactive)?;
                        loop {
                            let inertia = fiber.inner.inertia.borrow().clone();
                            match inertia {
                                Some(task) => task.await,
                                None => break,
                            }
                        }
                        fiber.release();
                        Ok(())
                    })))
                }))))
            })
        };
        let dispose = parent.effect(execute, "ctx.plugin()")?;
        *fiber.inner.dispose.borrow_mut() = Some(dispose);
        Ok(fiber)
    }
}

/// A fiber held without keeping it alive.
pub struct WeakFiber<H: Host> {
    inner: Weak<FiberInner<H>>,
}

impl<H: Host> Clone for WeakFiber<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<H: Host> WeakFiber<H> {
    pub fn upgrade(&self) -> Option<Fiber<H>> {
        self.inner.upgrade().map(|inner| Fiber { inner })
    }
}

/// Re-exported for the registry: the pending disposal of a fiber.
pub type Disposal<H> = Pending<H>;
