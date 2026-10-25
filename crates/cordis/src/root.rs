//! The root of a context tree: everything a Cordis app shares.
//!
//! `new Context()` makes one: the registry of plugin runtimes, the event hooks, the service
//! store and the property declarations, and the root fiber. Every fiber, effect and hook of the
//! tree points back here (weakly — the root owns them, not the other way round).

use std::cell::{Cell, RefCell};
use std::rc::{Rc, Weak};

use indexmap::IndexMap;

use crate::fiber::{Epoch, Fiber, FiberInner, FiberState};
use crate::host::{Host, NativeFn};
use crate::list::DisposableList;

/// A listener: a host function, or one of the engine's own.
pub enum Callback<H: Host> {
    Value(H::Value),
    Native(NativeFn<H>),
}

impl<H: Host> Clone for Callback<H> {
    fn clone(&self) -> Self {
        match self {
            Self::Value(value) => Self::Value(value.clone()),
            Self::Native(f) => Self::Native(f.clone()),
        }
    }
}

impl<H: Host> Callback<H> {
    pub fn call(&self, host: &H, this: &H::Value, args: &[H::Value]) -> Result<H::Value, H::Error> {
        match self {
            Self::Value(f) => host.call(f, this, args),
            Self::Native(f) => f(this, args),
        }
    }

    pub fn same(&self, other: &Self, host: &H) -> bool {
        match (self, other) {
            (Self::Value(a), Self::Value(b)) => host.same(a, b),
            (Self::Native(a), Self::Native(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }

    /// As a host value: the engine's own become host functions.
    pub fn to_value(&self, host: &H, name: &str) -> H::Value {
        match self {
            Self::Value(value) => value.clone(),
            Self::Native(f) => host.native_function(name, f.clone()),
        }
    }
}

/// One registered listener: `{ ctx, callback, global, prepend }`.
pub struct Hook<H: Host> {
    pub ctx: H::Ctx,
    pub callback: Callback<H>,
    pub global: bool,
    pub prepend: bool,
    /// The options object as the listener was registered with it, for hosts that show it.
    pub options: Option<H::Value>,
}

/// A provided service: `{ name, value, fiber, check }`. `reflect.set` replaces the value in
/// place, so every store snapshot holding the impl sees it.
pub struct Impl<H: Host> {
    pub name: String,
    pub value: RefCell<H::Value>,
    pub fiber: Fiber<H>,
    pub check: Option<H::Value>,
}

/// How a property name is declared on contexts.
pub enum Prop<H: Host> {
    /// `ctx.provide(name, ...)` declared it: it resolves to a service.
    Service,
    /// `ctx.accessor(name, { get, set })`: it runs these.
    Accessor {
        get: Option<Callback<H>>,
        set: Option<Callback<H>>,
    },
}

impl<H: Host> Clone for Prop<H> {
    fn clone(&self) -> Self {
        match self {
            Self::Service => Self::Service,
            Self::Accessor { get, set } => Self::Accessor {
                get: get.clone(),
                set: set.clone(),
            },
        }
    }
}

impl<H: Host> Prop<H> {
    /// `"service"` or `"accessor"`, as error messages name them.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Service => "service",
            Self::Accessor { .. } => "accessor",
        }
    }
}

/// A plugin as the registry knows it: `{ name, callback, fibers, Config }`.
pub struct Runtime<H: Host> {
    pub(crate) inner: Rc<RuntimeInner<H>>,
}

impl<H: Host> Clone for Runtime<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

pub(crate) struct RuntimeInner<H: Host> {
    pub(crate) key: H::PluginKey,
    pub(crate) name: Option<String>,
    pub(crate) callback: H::Value,
    pub(crate) schema: Option<H::Value>,
    pub(crate) fibers: DisposableList<Fiber<H>>,
    /// The host's face, made once.
    pub(crate) face: RefCell<Option<H::Value>>,
}

impl<H: Host> Runtime<H> {
    pub fn name(&self) -> Option<&str> {
        self.inner.name.as_deref()
    }

    pub fn callback(&self) -> &H::Value {
        &self.inner.callback
    }

    pub fn key(&self) -> &H::PluginKey {
        &self.inner.key
    }

    /// `Config`.
    pub fn schema(&self) -> Option<&H::Value> {
        self.inner.schema.as_ref()
    }

    /// Its live fibers, in the order they were made.
    pub fn fibers(&self) -> Vec<Fiber<H>> {
        self.inner.fibers.values()
    }

    pub fn face(&self) -> Option<H::Value> {
        self.inner.face.borrow().clone()
    }

    pub fn set_face(&self, face: H::Value) {
        *self.inner.face.borrow_mut() = Some(face);
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

/// The shared state of a context tree.
pub struct RootInner<H: Host> {
    pub host: H,
    pub(crate) counter: Cell<u32>,
    pub(crate) runtimes: RefCell<IndexMap<H::PluginKey, Runtime<H>>>,
    pub(crate) hooks: RefCell<IndexMap<String, Vec<Rc<Hook<H>>>>>,
    pub(crate) store: RefCell<IndexMap<H::Key, Rc<Impl<H>>>>,
    pub(crate) props: RefCell<IndexMap<String, Prop<H>>>,
    pub(crate) fiber: RefCell<Option<Fiber<H>>>,
    pub(crate) ctx: RefCell<Option<H::Ctx>>,
}

/// A handle on a context tree's root.
pub struct Root<H: Host> {
    pub(crate) inner: Rc<RootInner<H>>,
}

impl<H: Host> Clone for Root<H> {
    fn clone(&self) -> Self {
        Self {
            inner: self.inner.clone(),
        }
    }
}

impl<H: Host> Root<H> {
    /// Makes a root around `host`. The root context is given afterwards with
    /// [`Root::set_ctx`], since a host usually needs the root to make it.
    pub fn new(host: H) -> Self {
        let inner = Rc::new(RootInner {
            host,
            counter: Cell::new(0),
            runtimes: RefCell::new(IndexMap::new()),
            hooks: RefCell::new(IndexMap::new()),
            store: RefCell::new(IndexMap::new()),
            props: RefCell::new(IndexMap::new()),
            fiber: RefCell::new(None),
            ctx: RefCell::new(None),
        });
        let fiber = Fiber {
            inner: Rc::new(FiberInner::root(Rc::downgrade(&inner))),
        };
        *inner.fiber.borrow_mut() = Some(fiber);
        Self { inner }
    }

    pub(crate) fn from_inner(inner: Rc<RootInner<H>>) -> Self {
        Self { inner }
    }

    pub fn downgrade(&self) -> Weak<RootInner<H>> {
        Rc::downgrade(&self.inner)
    }

    pub fn host(&self) -> &H {
        &self.inner.host
    }

    pub fn ptr_eq(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }

    /// The root context. Hosts set it once, right after [`Root::new`]; the events service's
    /// two built-in listeners are registered under it then.
    pub fn set_ctx(&self, ctx: H::Ctx) {
        *self.inner.ctx.borrow_mut() = Some(ctx.clone());
        let fiber = self.fiber();
        *fiber.inner.ctx.borrow_mut() = Some(ctx.clone());
        *fiber.inner.parent_ctx.borrow_mut() = Some(ctx.clone());
        self.install_builtin_hooks(&ctx);
    }

    pub fn ctx(&self) -> Option<H::Ctx> {
        self.inner.ctx.borrow().clone()
    }

    /// The root fiber: uid 0, always active, whose disposal is a restart.
    pub fn fiber(&self) -> Fiber<H> {
        self.inner
            .fiber
            .borrow()
            .clone()
            .expect("a root has its fiber")
    }

    /// `registry.counter`: the next uid.
    pub(crate) fn next_uid(&self) -> u32 {
        let uid = self.inner.counter.get() + 1;
        self.inner.counter.set(uid);
        uid
    }

    /// `registry.counter`: takes the next uid, as reading the getter does.
    pub fn next_counter(&self) -> u32 {
        self.next_uid()
    }

    /// `registry.size`.
    pub fn runtime_count(&self) -> usize {
        self.inner.runtimes.borrow().len()
    }

    /// The runtimes, in registration order.
    pub fn runtimes(&self) -> Vec<Runtime<H>> {
        self.inner.runtimes.borrow().values().cloned().collect()
    }

    /// `registry.get(plugin)`.
    pub fn runtime(&self, key: &H::PluginKey) -> Option<Runtime<H>> {
        self.inner.runtimes.borrow().get(key).cloned()
    }

    /// `registry.delete(plugin)`: forgets the runtime and disposes every fiber it has.
    pub fn delete_runtime(&self, key: &H::PluginKey) -> Option<Runtime<H>> {
        let runtime = self.inner.runtimes.borrow_mut().shift_remove(key)?;
        // Its face holds it (through its handle); a forgotten runtime keeps neither.
        runtime.inner.face.borrow_mut().take();
        for fiber in runtime.fibers() {
            if let Some(dispose) = fiber.dispose_effect() {
                if let Err(error) = dispose.dispose() {
                    fiber.report(&error);
                }
            }
        }
        Some(runtime)
    }

    /// Every fiber of every runtime, as `notify` walks them.
    pub(crate) fn all_fibers(&self) -> Vec<Fiber<H>> {
        self.runtimes()
            .into_iter()
            .flat_map(|runtime| runtime.fibers())
            .collect()
    }

    /// `Object.keys(ctx.events._hooks)` with their lists.
    pub fn hooks(&self) -> Vec<(String, Vec<Rc<Hook<H>>>)> {
        self.inner
            .hooks
            .borrow()
            .iter()
            .map(|(name, list)| (name.clone(), list.clone()))
            .collect()
    }

    /// The hooks of one event, in call order.
    pub fn hooks_of(&self, name: &str) -> Vec<Rc<Hook<H>>> {
        self.inner
            .hooks
            .borrow()
            .get(name)
            .cloned()
            .unwrap_or_default()
    }

    /// A declared property.
    pub fn prop(&self, name: &str) -> Option<Prop<H>> {
        self.inner.props.borrow().get(name).cloned()
    }

    /// Every declared property, in declaration order.
    pub fn props(&self) -> Vec<(String, Prop<H>)> {
        self.inner
            .props
            .borrow()
            .iter()
            .map(|(name, prop)| (name.clone(), prop.clone()))
            .collect()
    }

    /// Every provided service, in the order they were provided.
    pub fn impls(&self) -> Vec<(H::Key, Rc<Impl<H>>)> {
        self.inner
            .store
            .borrow()
            .iter()
            .map(|(key, imp)| (key.clone(), imp.clone()))
            .collect()
    }

    pub(crate) fn report(&self, ctx: Option<&H::Ctx>, error: &H::Error) {
        let ctx = ctx.cloned().or_else(|| self.ctx());
        if let Some(ctx) = ctx {
            self.inner.host.report(&ctx, error);
        }
    }
}

impl<H: Host> FiberInner<H> {
    /// The root fiber: uid 0, active, an empty store, epoch `""`, no runtime.
    pub(crate) fn root(root: Weak<RootInner<H>>) -> Self {
        let fiber = Self::blank(root, None, Vec::new(), None);
        fiber.uid.set(Some(0));
        fiber.state.set(FiberState::Active);
        *fiber.store.borrow_mut() = Some(Default::default());
        *fiber.epoch.borrow_mut() = Epoch::Active(String::new());
        fiber
    }
}
