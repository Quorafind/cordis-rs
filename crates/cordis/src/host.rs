//! What the engine asks of the language it runs plugins in.
//!
//! Cordis is dynamically typed: configs, services, event arguments and listeners are whatever
//! the host language has. The engine is generic over a [`Host`] that owns those values and
//! answers the handful of questions the algorithms need — call this, await that, is this
//! nullish, which isolation key does this context give `name` — so a JavaScript engine can
//! hand it `JsValue`s and a Rust program its own types, with nothing converted on the way.

use std::hash::Hash;
use std::rc::Rc;

use crate::effect::{Disposer, Effect};
use crate::fiber::Fiber;
use crate::task::{LocalBoxFuture, Scope};

/// A value-level function the engine makes for the host: the `next` a waterfall listener
/// receives, the two built-in listeners the events service installs.
pub type NativeFn<H> = Rc<
    dyn Fn(
        &<H as Host>::Value,
        &[<H as Host>::Value],
    ) -> Result<<H as Host>::Value, <H as Host>::Error>,
>;

/// Which constructor an engine-made error has.
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum ErrorKind {
    /// `Error`.
    Error,
    /// `TypeError`.
    TypeError,
    /// `CordisError` with `code`.
    Cordis(&'static str),
}

/// What an effect's body answered, sorted into the shapes `Fiber._execute` accepts.
pub enum EffectValue<H: Host> {
    /// `null` or `undefined`: nothing to dispose.
    Nullish,
    /// A function: the disposer.
    Dispose(Disposer<H>),
    /// A thenable: what it settles with is collected as above.
    Thenable(LocalBoxFuture<Result<H::Value, H::Error>>),
    /// A synchronous iterator (a generator): every value it yields, and the one it returns,
    /// is collected.
    Iter(Box<dyn EffectIter<H>>),
    /// An async iterator, collected the same way while the effect is still current.
    AsyncIter(Box<dyn AsyncEffectIter<H>>),
    /// Anything else: `TypeError: Invalid effect`.
    Invalid,
}

/// A host iterator, stepped by the engine. `next` answers `(value, done)`.
pub trait EffectIter<H: Host> {
    fn next(&mut self) -> Result<(H::Value, bool), H::Error>;
}

/// A host async iterator, stepped by the engine.
pub trait AsyncEffectIter<H: Host> {
    fn next(&mut self) -> LocalBoxFuture<Result<(H::Value, bool), H::Error>>;
}

/// The language plugins are written in.
///
/// Every method is called with no engine state borrowed, so a host may call back into the
/// engine from any of them (a listener that emits, a disposer that provides).
pub trait Host: Sized + 'static {
    /// Any value: a config, a service, an argument, a listener, a disposer.
    type Value: Clone + 'static;
    /// A context, as the host represents one.
    type Ctx: Clone + 'static;
    /// An isolation label: what `ctx[Context.isolate][name]` holds.
    type Key: Clone + Eq + Hash + 'static;
    /// A thrown value.
    type Error: Clone + 'static;
    /// What the registry keys a plugin's runtime by: the identity of its callback.
    type PluginKey: Clone + Eq + Hash + 'static;

    // ─── Values ──────────────────────────────────────────────────────────────────────────

    fn undefined(&self) -> Self::Value;
    fn null(&self) -> Self::Value;
    fn string(&self, text: &str) -> Self::Value;
    fn number(&self, number: f64) -> Self::Value;
    fn boolean(&self, value: bool) -> Self::Value;
    fn array(&self, items: Vec<Self::Value>) -> Self::Value;
    /// `null` or `undefined`.
    fn is_nullish(&self, value: &Self::Value) -> bool;
    /// What `bail` and `serial` stop at: anything but `null`, `false` and `undefined`.
    fn is_bailed(&self, value: &Self::Value) -> bool;
    /// JavaScript truthiness: what a filter's answer is read as.
    fn is_truthy(&self, value: &Self::Value) -> bool;
    /// Identity (`===` for objects).
    fn same(&self, a: &Self::Value, b: &Self::Value) -> bool;

    /// The host's face of a fiber — what listeners of `internal/plugin` receive.
    fn fiber_value(&self, fiber: &Fiber<Self>) -> Self::Value;
    /// The fiber a value is the face of, if it is one.
    fn fiber_from_value(&self, value: &Self::Value) -> Option<Fiber<Self>>;
    /// The host's face of an effect: its disposer, as a function value.
    fn effect_value(&self, effect: &Effect<Self>) -> Self::Value;
    /// A context as a value (a `this`, an argument).
    fn ctx_value(&self, ctx: &Self::Ctx) -> Self::Value;
    /// A function value that runs `f`.
    fn native_function(&self, name: &str, f: NativeFn<Self>) -> Self::Value;
    /// The value an event name stands for (a JS host keeps symbols as names).
    fn event_name(&self, name: &str) -> Self::Value {
        self.string(name)
    }

    // ─── Calling ─────────────────────────────────────────────────────────────────────────

    /// `Reflect.apply(f, this, args)`.
    fn call(
        &self,
        f: &Self::Value,
        this: &Self::Value,
        args: &[Self::Value],
    ) -> Result<Self::Value, Self::Error>;
    /// Whether `await value` would wait on it: an object with a `then`.
    fn is_thenable(&self, value: &Self::Value) -> bool;
    /// `await value`, as a future.
    fn await_value(&self, value: Self::Value) -> LocalBoxFuture<Result<Self::Value, Self::Error>>;
    /// A task as the host's promise: settles as the task does.
    fn promise(&self, task: crate::task::Task<Result<Self::Value, Self::Error>>) -> Self::Value;
    /// The async scope the caller is in, for a task to resume in.
    fn scope(&self) -> Option<Rc<dyn Scope>> {
        None
    }

    // ─── Errors ──────────────────────────────────────────────────────────────────────────

    fn error(&self, kind: ErrorKind, message: &str) -> Self::Error;
    /// `new AggregateError(errors)`.
    fn aggregate_error(&self, errors: Vec<Self::Error>) -> Self::Error;
    /// What `composeError` makes of a thrown value: an `Error` stays itself; anything else
    /// becomes `new Error(String(value))`.
    fn compose_error(&self, error: Self::Error) -> Self::Error {
        error
    }
    /// `ctx.logger.error(error)`.
    fn report(&self, ctx: &Self::Ctx, error: &Self::Error);

    // ─── Contexts ────────────────────────────────────────────────────────────────────────

    /// `ctx.fiber`.
    fn fiber_of(&self, ctx: &Self::Ctx) -> Option<Fiber<Self>>;
    /// A new fiber's own context: `parent.extend({ fiber })`, with an intercept layer for
    /// every injected service that has a config (`inject` lists `(name, config)`, configs
    /// non-null only).
    fn derive_fiber_ctx(
        &self,
        parent: &Self::Ctx,
        fiber: &Fiber<Self>,
        inject: &[(String, Self::Value)],
        has_inject: bool,
    ) -> Self::Ctx;
    /// `ctx[Context.isolate][name]`.
    fn isolate_key(&self, ctx: &Self::Ctx, name: &str) -> Option<Self::Key>;
    /// `root[Context.isolate][name]`, without making one.
    fn root_key(&self, name: &str) -> Option<Self::Key>;
    /// `root[Context.isolate][name] ??= Symbol(name)`.
    fn ensure_root_key(&self, name: &str) -> Self::Key;

    // ─── Plugins, effects, services ──────────────────────────────────────────────────────

    /// Runs a plugin's body for `ctx`: `new callback(ctx, config)` (with its init hooks and
    /// `[Service.init]`) for a constructor, `callback(ctx, config)` otherwise. Answers what
    /// it returned, for [`Host::classify`].
    fn execute_plugin(
        &self,
        callback: &Self::Value,
        ctx: &Self::Ctx,
        config: &Self::Value,
    ) -> Result<Self::Value, Self::Error>;
    /// `resolveConfig`: validates `config` with the runtime's `Config` (a Standard Schema),
    /// when it has one.
    fn resolve_config(
        &self,
        schema: Option<&Self::Value>,
        config: Self::Value,
    ) -> Result<Self::Value, Self::Error>;
    /// Sorts what an effect's body answered into the shapes `_execute` accepts.
    fn classify(&self, value: Self::Value) -> EffectValue<Self>;
    /// A collected value as a disposer: a function is one; `None` otherwise.
    fn as_disposer(&self, value: &Self::Value) -> Option<Disposer<Self>>;
    /// A service's `check`, called on the service as `ctx` sees it.
    fn check_service(
        &self,
        check: &Self::Value,
        ctx: &Self::Ctx,
        service: &Self::Value,
    ) -> Result<bool, Self::Error>;

    // ─── Events ──────────────────────────────────────────────────────────────────────────

    /// `this?.[Context.filter]`, when it is there.
    fn event_filter(&self, this: &Self::Value) -> Option<Self::Value>;
}
