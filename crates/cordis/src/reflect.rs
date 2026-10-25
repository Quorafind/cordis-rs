//! Services and property declarations: `provide`, `accessor`, `get`, `set`, `notify`, and the
//! walk a context's property read takes to find a service.
//!
//! A service is stored under the *isolation key* its name has where it is provided —
//! `ctx[Context.isolate][name]`, the root's own key unless an `isolate` narrowed it. Fibers
//! injecting the name under the same key are notified when it comes or goes, check it, and
//! move to a new epoch (which reloads them).

use std::rc::Rc;

use crate::effect::{Disposer, Effect};
use crate::events::{Filter, quote};
use crate::fiber::{Fiber, FiberState, PluginSpec};
use crate::host::{EffectValue, ErrorKind, Host};
use crate::root::{Callback, Impl, Prop, Root, Runtime, RuntimeInner};
use crate::task;

/// Where a context's read of a service name ended.
pub enum Lookup<H: Host> {
    /// The service.
    Found(H::Value),
    /// A fiber on the way injects the name but is not active: "cannot get required service
    /// in inactive context".
    Inactive,
    /// Nothing provides it here: "cannot get property without inject".
    Missing,
}

impl<H: Host> Root<H> {
    /// `_getImpl(name, strict)`: the impl `ctx` sees under `name`; with `strict`, only if its
    /// provider is active.
    pub fn get_impl(&self, ctx: &H::Ctx, name: &str, strict: bool) -> Option<Rc<Impl<H>>> {
        let key = self.host().isolate_key(ctx, name)?;
        let imp = self.inner.store.borrow().get(&key).cloned()?;
        if strict && imp.fiber.state() != FiberState::Active {
            return None;
        }
        Some(imp)
    }

    /// `reflect.get(name, strict)`, untraced.
    pub fn get_service(&self, ctx: &H::Ctx, name: &str, strict: bool) -> Option<H::Value> {
        self.get_impl(ctx, name, strict)
            .map(|imp| imp.value.borrow().clone())
    }

    /// `reflect.set(name, value)`: replaces the value of a service `ctx`'s own fiber provides.
    pub fn set_service(&self, ctx: &H::Ctx, name: &str, value: H::Value) -> Result<(), H::Error> {
        let host = self.host();
        let imp = host
            .isolate_key(ctx, name)
            .and_then(|key| self.inner.store.borrow().get(&key).cloned());
        let Some(imp) = imp else {
            return Err(host.error(
                ErrorKind::Error,
                &format!("cannot set property {} without provide", quote(name)),
            ));
        };
        let fiber = host.fiber_of(ctx);
        if fiber.as_ref() != Some(&imp.fiber) {
            return Err(host.error(
                ErrorKind::Error,
                &format!("cannot set property {} in multiple fibers", quote(name)),
            ));
        }
        *imp.value.borrow_mut() = value;
        Ok(())
    }

    /// `reflect.provide(name, value, check)`: an effect of `ctx`'s fiber that stores the
    /// service and notifies its injectors; disposing it removes the service, notifies again,
    /// and waits for every fiber that reacted.
    pub fn provide(
        &self,
        ctx: &H::Ctx,
        name: &str,
        value: H::Value,
        check: Option<H::Value>,
    ) -> Result<Effect<H>, H::Error> {
        let host = self.host();
        let fiber = host.fiber_of(ctx).expect("a context has its fiber");
        let root = self.downgrade();
        let ctx = ctx.clone();
        let name = name.to_owned();
        let label = format!("ctx.provide({})", quote(&name));
        let execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>> = Rc::new(move || {
            let root = root
                .upgrade()
                .map(Root::from_inner)
                .expect("an effect outlived its root");
            let host = root.host();
            {
                let mut props = root.inner.props.borrow_mut();
                match props.get(&name) {
                    None | Some(Prop::Service) => {}
                    Some(other) => {
                        return Err(host.error(
                            ErrorKind::Error,
                            &format!(
                                "property {} is already declared as {}",
                                quote(&name),
                                other.kind()
                            ),
                        ));
                    }
                }
                props.insert(name.clone(), Prop::Service);
            }
            let root_key = host.ensure_root_key(&name);
            let key = host.isolate_key(&ctx, &name).unwrap_or(root_key);
            let fiber = host.fiber_of(&ctx).expect("a context has its fiber");
            if let Some(existing) = root.inner.store.borrow().get(&key) {
                return Err(host.error(
                    ErrorKind::Error,
                    &format!(
                        "service {} has been registered at <{}>",
                        quote(&name),
                        existing.fiber.name()
                    ),
                ));
            }
            let imp = Rc::new(Impl {
                name: name.clone(),
                value: std::cell::RefCell::new(value.clone()),
                fiber: fiber.clone(),
                check: check.clone(),
            });
            root.inner
                .store
                .borrow_mut()
                .insert(key.clone(), imp.clone());
            {
                let mut store = fiber.inner.store.borrow_mut();
                match store.as_mut() {
                    Some(store) => {
                        store.insert(name.clone(), imp);
                    }
                    None => {
                        drop(store);
                        root.inner.store.borrow_mut().shift_remove(&key);
                        return Err(host.error(
                            ErrorKind::TypeError,
                            &format!(
                                "Cannot set properties of undefined (setting {})",
                                quote(&name)
                            ),
                        ));
                    }
                }
            }
            if fiber.state() == FiberState::Active {
                root.notify(&ctx, std::slice::from_ref(&name))?;
            }
            let weak = root.downgrade();
            let ctx = ctx.clone();
            let name = name.clone();
            Ok(EffectValue::Dispose(Disposer::Native(Rc::new(move || {
                let Some(root) = weak.upgrade().map(Root::from_inner) else {
                    return Ok(None);
                };
                let fiber = fiber.clone();
                let ctx = ctx.clone();
                let name = name.clone();
                let key = key.clone();
                let scope = root.host().scope();
                Ok(Some(task::spawn_in(scope, async move {
                    root.inner.store.borrow_mut().shift_remove(&key);
                    let fibers = root.notify(&ctx, std::slice::from_ref(&name))?;
                    for fiber in fibers {
                        let _ = fiber.wait().await;
                    }
                    if let Some(store) = fiber.inner.store.borrow_mut().as_mut() {
                        store.shift_remove(&name);
                    }
                    Ok(())
                })))
            }))))
        });
        fiber.effect(execute, label)
    }

    /// `reflect.notify(names)`: every fiber injecting one of `names` under the key `ctx` gives
    /// it re-checks it and refreshes its epoch; then `internal/service` tells listeners that
    /// share the key. Answers the fibers that reacted.
    pub fn notify(&self, ctx: &H::Ctx, names: &[String]) -> Result<Vec<Fiber<H>>, H::Error> {
        let host = self.host();
        let keys: Vec<Option<H::Key>> = names.iter().map(|n| host.isolate_key(ctx, n)).collect();
        let mut fibers = Vec::new();
        for fiber in self.all_fibers() {
            let mut touched = false;
            for (name, key) in names.iter().zip(&keys) {
                if !fiber.injects(name) {
                    continue;
                }
                let Some(own) = fiber.ctx() else { continue };
                if host.isolate_key(&own, name) != *key {
                    continue;
                }
                touched = true;
                fiber.check_impl(name);
            }
            if !touched {
                continue;
            }
            fiber.refresh()?;
            fibers.push(fiber);
        }
        for (name, key) in names.iter().zip(keys) {
            let value = self
                .get_service(ctx, name, false)
                .unwrap_or_else(|| host.undefined());
            let root = self.downgrade();
            let name_owned = name.clone();
            let filter = Filter::Native(Rc::new(move |target: &H::Ctx| {
                let Some(root) = root.upgrade().map(Root::from_inner) else {
                    return false;
                };
                root.host().isolate_key(target, &name_owned) == key
            }));
            self.emit_filtered(
                Some(host.ctx_value(ctx)),
                "internal/service",
                &[host.string(name), value],
                &filter,
            )?;
        }
        Ok(fibers)
    }

    /// `reflect.accessor(name, { get, set })`.
    pub fn accessor(
        &self,
        ctx: &H::Ctx,
        name: &str,
        get: Option<Callback<H>>,
        set: Option<Callback<H>>,
    ) -> Result<Effect<H>, H::Error> {
        let host = self.host();
        let fiber = host.fiber_of(ctx).expect("a context has its fiber");
        let root = self.downgrade();
        let name = name.to_owned();
        let label = format!("ctx.accessor({})", quote(&name));
        let execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>> = Rc::new(move || {
            let root = root
                .upgrade()
                .map(Root::from_inner)
                .expect("an effect outlived its root");
            {
                let mut props = root.inner.props.borrow_mut();
                if let Some(existing) = props.get(&name) {
                    return Err(root.host().error(
                        ErrorKind::Error,
                        &format!(
                            "property {} is already declared as {}",
                            quote(&name),
                            existing.kind()
                        ),
                    ));
                }
                props.insert(
                    name.clone(),
                    Prop::Accessor {
                        get: get.clone(),
                        set: set.clone(),
                    },
                );
            }
            let weak = root.downgrade();
            let name = name.clone();
            Ok(EffectValue::Dispose(Disposer::Native(Rc::new(move || {
                if let Some(root) = weak.upgrade().map(Root::from_inner) {
                    root.inner.props.borrow_mut().shift_remove(&name);
                }
                Ok(None)
            }))))
        });
        fiber.effect(execute, label)
    }

    /// The service walk of a context's property read: from `fiber` up, the first store that
    /// has `name`, as long as each step up shares the root's isolation key for it.
    pub fn lookup(&self, fiber: &Fiber<H>, name: &str) -> Lookup<H> {
        let host = self.host();
        let key = host.root_key(name);
        let mut fiber = fiber.clone();
        loop {
            if let Some(imp) = fiber.stored(name) {
                return Lookup::Found(imp.value.borrow().clone());
            }
            if fiber.injects(name) {
                return Lookup::Inactive;
            }
            if fiber.is_root() {
                return Lookup::Missing;
            }
            let Some(parent_ctx) = fiber.parent_ctx() else {
                return Lookup::Missing;
            };
            if host.isolate_key(&parent_ctx, name) != key {
                return Lookup::Missing;
            }
            match host.fiber_of(&parent_ctx) {
                Some(parent) => fiber = parent,
                None => return Lookup::Missing,
            }
        }
    }

    // ─── The registry ────────────────────────────────────────────────────────────────────

    /// `registry.plugin(plugin, config)` on `ctx`: the plugin's runtime (made the first time),
    /// and a new fiber of it on `ctx`.
    pub fn plugin(
        &self,
        ctx: &H::Ctx,
        spec: PluginSpec<H>,
        config: H::Value,
    ) -> Result<Fiber<H>, H::Error> {
        let host = self.host();
        let parent = host.fiber_of(ctx).expect("a context has its fiber");
        parent.assert_active()?;
        let existing = self.runtime(&spec.key);
        let runtime = match existing {
            Some(runtime) => runtime,
            None => {
                crate::stats::add(&crate::stats::RUNTIMES, 1);
                let runtime = Runtime {
                    inner: Rc::new(RuntimeInner {
                        key: spec.key.clone(),
                        name: spec.name,
                        callback: spec.callback,
                        schema: spec.schema,
                        fibers: Default::default(),
                        deleted: Default::default(),
                        face: Default::default(),
                    }),
                };
                self.inner
                    .runtimes
                    .borrow_mut()
                    .insert(spec.key.clone(), runtime.clone());
                runtime
            }
        };
        Fiber::plugin(self, ctx, &parent, runtime, spec.inject, config)
    }

    /// Wraps a finished-or-pending unit task as the host's promise of `undefined`.
    pub fn task_value(&self, pending: task::Task<Result<(), H::Error>>) -> H::Value {
        let host = self.host();
        let undefined = host.undefined();
        let mapped = task::spawn(async move { pending.await.map(|()| undefined) });
        host.promise(mapped)
    }
}
