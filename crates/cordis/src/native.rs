//! Cordis for Rust programs: the engine on Rust values, with no JavaScript anywhere.
//!
//! ```
//! use cordis::native::{Context, Value};
//!
//! let root = Context::new();
//! root.provide("greeting", Value::string("hello")).unwrap();
//! let fiber = root
//!     .inject(&["greeting"], |ctx, _| {
//!         let greeting = ctx.get("greeting")?.unwrap();
//!         assert_eq!(greeting.as_str(), Some("hello"));
//!         Ok(Value::Undefined)
//!     })
//!     .unwrap();
//! cordis::task::run_until_stalled();
//! assert_eq!(fiber.state(), cordis::FiberState::Active);
//! ```
//!
//! Values are a small dynamic [`Value`]: primitives, functions, futures (as promises), fibers,
//! contexts, effects, and `Rc<dyn Any>` for a plugin's own types. A plugin is anything
//! implementing [`Plugin`], or a closure given to [`Context::plugin_fn`].

use std::any::Any;
use std::cell::{Cell, RefCell};
use std::collections::HashMap;
use std::fmt;
use std::rc::Rc;

use crate::effect::{Disposer, Effect};
use crate::events::{Filter, OnOptions};
use crate::fiber::{Fiber, PluginSpec};
use crate::host::{EffectIter, EffectValue, ErrorKind, Host, NativeFn};
use crate::reflect::Lookup;
use crate::root::{Callback, Root};
use crate::task::{LocalBoxFuture, Task};

// ─── Values ──────────────────────────────────────────────────────────────────────────────

/// A function value: `this` and arguments in, a value out.
pub type Function = Rc<dyn Fn(&Value, &[Value]) -> Result<Value, Error>>;

/// A dynamically typed value.
#[derive(Clone, Default)]
pub enum Value {
    #[default]
    Undefined,
    Null,
    Bool(bool),
    Number(f64),
    String(Rc<str>),
    Array(Rc<Vec<Value>>),
    Function(Function),
    /// A value of the plugin's own type.
    Object(Rc<dyn Any>),
    Fiber(Fiber<NativeHost>),
    Context(Context),
    /// An effect, as its disposer.
    Effect(Effect<NativeHost>),
    /// A pending or settled result.
    Promise(Task<Result<Value, Error>>),
    /// A plugin.
    Plugin(Rc<dyn Plugin>),
}

impl Value {
    pub fn string(text: &str) -> Self {
        Self::String(text.into())
    }

    pub fn object<T: Any>(value: T) -> Self {
        Self::Object(Rc::new(value))
    }

    pub fn function(f: impl Fn(&Value, &[Value]) -> Result<Value, Error> + 'static) -> Self {
        Self::Function(Rc::new(f))
    }

    /// A disposer: a function of no arguments.
    pub fn disposer(f: impl Fn() + 'static) -> Self {
        Self::function(move |_, _| {
            f();
            Ok(Value::Undefined)
        })
    }

    /// A promise of what `future` settles with.
    pub fn promise(
        future: impl std::future::Future<Output = Result<Value, Error>> + 'static,
    ) -> Self {
        Self::Promise(crate::task::spawn(future))
    }

    pub fn as_str(&self) -> Option<&str> {
        match self {
            Self::String(text) => Some(text),
            _ => None,
        }
    }

    pub fn as_number(&self) -> Option<f64> {
        match self {
            Self::Number(n) => Some(*n),
            _ => None,
        }
    }

    /// The plugin's own value, if it is a `T`.
    pub fn downcast<T: Any>(&self) -> Option<Rc<T>> {
        match self {
            Self::Object(any) => any.clone().downcast::<T>().ok(),
            _ => None,
        }
    }

    pub fn is_nullish(&self) -> bool {
        matches!(self, Self::Undefined | Self::Null)
    }

    pub fn is_truthy(&self) -> bool {
        match self {
            Self::Undefined | Self::Null => false,
            Self::Bool(b) => *b,
            Self::Number(n) => *n != 0. && !n.is_nan(),
            Self::String(s) => !s.is_empty(),
            _ => true,
        }
    }

    /// Identity for references, equality for primitives (`===`).
    pub fn same(&self, other: &Self) -> bool {
        match (self, other) {
            (Self::Undefined, Self::Undefined) | (Self::Null, Self::Null) => true,
            (Self::Bool(a), Self::Bool(b)) => a == b,
            (Self::Number(a), Self::Number(b)) => a == b,
            (Self::String(a), Self::String(b)) => a == b,
            (Self::Array(a), Self::Array(b)) => Rc::ptr_eq(a, b),
            (Self::Function(a), Self::Function(b)) => Rc::ptr_eq(a, b),
            (Self::Object(a), Self::Object(b)) => Rc::ptr_eq(a, b),
            (Self::Fiber(a), Self::Fiber(b)) => a == b,
            (Self::Context(a), Self::Context(b)) => Rc::ptr_eq(&a.node, &b.node),
            (Self::Effect(a), Self::Effect(b)) => a.ptr_eq(b),
            (Self::Promise(a), Self::Promise(b)) => a.ptr_eq(b),
            (Self::Plugin(a), Self::Plugin(b)) => Rc::ptr_eq(a, b),
            _ => false,
        }
    }
}

impl fmt::Debug for Value {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            Self::Undefined => write!(f, "undefined"),
            Self::Null => write!(f, "null"),
            Self::Bool(b) => write!(f, "{b}"),
            Self::Number(n) => write!(f, "{n}"),
            Self::String(s) => write!(f, "{s:?}"),
            Self::Array(items) => f.debug_list().entries(items.iter()).finish(),
            Self::Function(_) => write!(f, "[Function]"),
            Self::Object(_) => write!(f, "[Object]"),
            Self::Fiber(fiber) => write!(f, "Fiber <{}>", fiber.name()),
            Self::Context(_) => write!(f, "Context"),
            Self::Effect(effect) => write!(f, "Effect {:?}", effect.label()),
            Self::Promise(_) => write!(f, "Promise"),
            Self::Plugin(plugin) => write!(f, "Plugin {:?}", plugin.name()),
        }
    }
}

impl From<&str> for Value {
    fn from(text: &str) -> Self {
        Self::string(text)
    }
}

impl From<f64> for Value {
    fn from(n: f64) -> Self {
        Self::Number(n)
    }
}

impl From<bool> for Value {
    fn from(b: bool) -> Self {
        Self::Bool(b)
    }
}

// ─── Errors ──────────────────────────────────────────────────────────────────────────────

/// A thrown value.
#[derive(Clone)]
pub struct Error {
    inner: Rc<ErrorInner>,
}

struct ErrorInner {
    name: String,
    code: Option<&'static str>,
    message: String,
    errors: Vec<Error>,
    value: Option<Value>,
}

impl Error {
    pub fn new(message: impl Into<String>) -> Self {
        Self::named("Error", message)
    }

    pub fn named(name: impl Into<String>, message: impl Into<String>) -> Self {
        Self {
            inner: Rc::new(ErrorInner {
                name: name.into(),
                code: None,
                message: message.into(),
                errors: Vec::new(),
                value: None,
            }),
        }
    }

    /// A non-error value thrown.
    pub fn thrown(value: Value) -> Self {
        Self {
            inner: Rc::new(ErrorInner {
                name: String::new(),
                code: None,
                message: format!("{value:?}"),
                errors: Vec::new(),
                value: Some(value),
            }),
        }
    }

    pub fn name(&self) -> &str {
        &self.inner.name
    }

    pub fn message(&self) -> &str {
        &self.inner.message
    }

    /// `CordisError`'s code.
    pub fn code(&self) -> Option<&'static str> {
        self.inner.code
    }

    /// An `AggregateError`'s errors.
    pub fn errors(&self) -> &[Error] {
        &self.inner.errors
    }

    pub fn same(&self, other: &Self) -> bool {
        Rc::ptr_eq(&self.inner, &other.inner)
    }
}

impl fmt::Debug for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.inner.name, self.inner.message)
    }
}

impl fmt::Display for Error {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        write!(f, "{}: {}", self.inner.name, self.inner.message)
    }
}

impl std::error::Error for Error {}

// ─── Contexts ────────────────────────────────────────────────────────────────────────────

/// An isolation label.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub struct Key(u64);

/// A context: a view of the tree with a fiber, and isolation and intercept layers.
#[derive(Clone)]
pub struct Context {
    node: Rc<Node>,
}

struct Node {
    parent: Option<Context>,
    root: Root<NativeHost>,
    fiber: Option<Fiber<NativeHost>>,
    isolate: Option<(String, Key)>,
    intercept: Option<(String, Value)>,
}

/// The host for Rust: see the module documentation.
pub struct NativeHost {
    keys: RefCell<HashMap<String, Key>>,
    next_key: Cell<u64>,
    reported: RefCell<Vec<Error>>,
    on_report: RefCell<Option<Rc<dyn Fn(&Error)>>>,
}

impl Default for NativeHost {
    fn default() -> Self {
        Self {
            keys: RefCell::new(HashMap::new()),
            next_key: Cell::new(1),
            reported: RefCell::new(Vec::new()),
            on_report: RefCell::new(None),
        }
    }
}

impl NativeHost {
    /// A new isolation label, as `Symbol(name)` is.
    pub fn key(&self) -> Key {
        let id = self.next_key.get();
        self.next_key.set(id + 1);
        Key(id)
    }

    /// What `ctx.logger.error` has been given, oldest first.
    pub fn reported(&self) -> Vec<Error> {
        self.reported.borrow().clone()
    }

    /// Calls `f` with every error reported from now on.
    pub fn on_report(&self, f: impl Fn(&Error) + 'static) {
        *self.on_report.borrow_mut() = Some(Rc::new(f));
    }
}

/// A plugin written in Rust.
pub trait Plugin: 'static {
    /// Its name, as `fiber.name` reports it.
    fn name(&self) -> Option<String> {
        None
    }

    /// The services it injects, each with an optional config.
    fn inject(&self) -> Vec<(String, Option<Value>)> {
        Vec::new()
    }

    /// Validates a config: what `Config` is to a JS plugin.
    fn config(&self, config: Value) -> Result<Value, Error> {
        Ok(config)
    }

    /// Runs it on `ctx`. What it answers is collected as an effect's body's answer: a
    /// disposer, an effect, a promise of one, or an array of them.
    fn apply(&self, ctx: &Context, config: &Value) -> Result<Value, Error>;
}

struct FnPlugin<F> {
    name: Option<String>,
    inject: Vec<(String, Option<Value>)>,
    apply: F,
}

impl<F: Fn(&Context, &Value) -> Result<Value, Error> + 'static> Plugin for FnPlugin<F> {
    fn name(&self) -> Option<String> {
        self.name.clone()
    }

    fn inject(&self) -> Vec<(String, Option<Value>)> {
        self.inject.clone()
    }

    fn apply(&self, ctx: &Context, config: &Value) -> Result<Value, Error> {
        (self.apply)(ctx, config)
    }
}

/// Makes a plugin of a closure.
pub fn plugin_fn(
    name: Option<&str>,
    inject: &[&str],
    apply: impl Fn(&Context, &Value) -> Result<Value, Error> + 'static,
) -> Rc<dyn Plugin> {
    Rc::new(FnPlugin {
        name: name.map(str::to_owned),
        inject: inject.iter().map(|n| (n.to_string(), None)).collect(),
        apply,
    })
}

struct ArrayIter {
    items: Rc<Vec<Value>>,
    at: usize,
}

impl EffectIter<NativeHost> for ArrayIter {
    fn next(&mut self) -> Result<(Value, bool), Error> {
        match self.items.get(self.at) {
            Some(item) => {
                self.at += 1;
                Ok((item.clone(), false))
            }
            None => Ok((Value::Undefined, true)),
        }
    }
}

impl Host for NativeHost {
    type Value = Value;
    type Ctx = Context;
    type Key = Key;
    type Error = Error;
    type PluginKey = usize;

    fn undefined(&self) -> Value {
        Value::Undefined
    }
    fn null(&self) -> Value {
        Value::Null
    }
    fn string(&self, text: &str) -> Value {
        Value::string(text)
    }
    fn number(&self, number: f64) -> Value {
        Value::Number(number)
    }
    fn boolean(&self, value: bool) -> Value {
        Value::Bool(value)
    }
    fn array(&self, items: Vec<Value>) -> Value {
        Value::Array(Rc::new(items))
    }
    fn is_nullish(&self, value: &Value) -> bool {
        value.is_nullish()
    }
    fn is_bailed(&self, value: &Value) -> bool {
        !matches!(value, Value::Undefined | Value::Null | Value::Bool(false))
    }
    fn is_truthy(&self, value: &Value) -> bool {
        value.is_truthy()
    }
    fn same(&self, a: &Value, b: &Value) -> bool {
        a.same(b)
    }
    fn fiber_value(&self, fiber: &Fiber<Self>) -> Value {
        Value::Fiber(fiber.clone())
    }
    fn fiber_from_value(&self, value: &Value) -> Option<Fiber<Self>> {
        match value {
            Value::Fiber(fiber) => Some(fiber.clone()),
            _ => None,
        }
    }
    fn effect_value(&self, effect: &Effect<Self>) -> Value {
        Value::Effect(effect.clone())
    }
    fn ctx_value(&self, ctx: &Context) -> Value {
        Value::Context(ctx.clone())
    }
    fn native_function(&self, _name: &str, f: NativeFn<Self>) -> Value {
        Value::Function(f)
    }
    fn call(&self, f: &Value, this: &Value, args: &[Value]) -> Result<Value, Error> {
        match f {
            Value::Function(f) => f(this, args),
            Value::Effect(effect) => {
                let pending = effect.dispose()?;
                Ok(match pending {
                    Some(task) => Value::Promise(crate::task::spawn(async move {
                        task.await.map(|()| Value::Undefined)
                    })),
                    None => Value::Undefined,
                })
            }
            other => Err(Error::named(
                "TypeError",
                format!("{other:?} is not a function"),
            )),
        }
    }
    fn is_thenable(&self, value: &Value) -> bool {
        matches!(value, Value::Promise(_))
    }
    fn await_value(&self, value: Value) -> LocalBoxFuture<Result<Value, Error>> {
        match value {
            Value::Promise(task) => Box::pin(task),
            other => Box::pin(async move {
                crate::task::yield_now().await;
                Ok(other)
            }),
        }
    }
    fn promise(&self, task: Task<Result<Value, Error>>) -> Value {
        Value::Promise(task)
    }
    fn error(&self, kind: ErrorKind, message: &str) -> Error {
        match kind {
            ErrorKind::Error => Error::new(message),
            ErrorKind::TypeError => Error::named("TypeError", message),
            ErrorKind::Cordis(code) => {
                let mut error = Error::named("CordisError", message);
                Rc::get_mut(&mut error.inner).expect("a new error").code = Some(code);
                error
            }
        }
    }
    fn aggregate_error(&self, errors: Vec<Error>) -> Error {
        Error {
            inner: Rc::new(ErrorInner {
                name: "AggregateError".into(),
                code: None,
                message: String::new(),
                errors,
                value: None,
            }),
        }
    }
    fn compose_error(&self, error: Error) -> Error {
        match &error.inner.value {
            Some(value) => Error::new(format!("{value:?}")),
            None => error,
        }
    }
    fn report(&self, _ctx: &Context, error: &Error) {
        self.reported.borrow_mut().push(error.clone());
        let hook = self.on_report.borrow().clone();
        if let Some(hook) = hook {
            hook(error);
        }
    }
    fn fiber_of(&self, ctx: &Context) -> Option<Fiber<Self>> {
        let mut node = Some(ctx);
        while let Some(current) = node {
            if let Some(fiber) = &current.node.fiber {
                return Some(fiber.clone());
            }
            node = current.node.parent.as_ref();
        }
        None
    }
    fn derive_fiber_ctx(
        &self,
        parent: &Context,
        fiber: &Fiber<Self>,
        inject: &[(String, Value)],
        _has_inject: bool,
    ) -> Context {
        let mut ctx = parent.derive(Some(fiber.clone()), None, None);
        for (name, config) in inject {
            ctx = ctx.derive(None, None, Some((name.clone(), config.clone())));
        }
        ctx
    }
    fn isolate_key(&self, ctx: &Context, name: &str) -> Option<Key> {
        let mut node = Some(ctx);
        while let Some(current) = node {
            if let Some((n, key)) = &current.node.isolate {
                if n == name {
                    return Some(key.clone());
                }
            }
            node = current.node.parent.as_ref();
        }
        self.root_key(name)
    }
    fn root_key(&self, name: &str) -> Option<Key> {
        self.keys.borrow().get(name).cloned()
    }
    fn ensure_root_key(&self, name: &str) -> Key {
        if let Some(key) = self.root_key(name) {
            return key;
        }
        let key = self.key();
        self.keys.borrow_mut().insert(name.to_owned(), key.clone());
        key
    }
    fn execute_plugin(
        &self,
        _runtime: &crate::root::Runtime<Self>,
        callback: &Value,
        ctx: &Context,
        config: &Value,
    ) -> Result<Value, Error> {
        match callback {
            Value::Plugin(plugin) => plugin.apply(ctx, config),
            other => self.call(
                other,
                &Value::Undefined,
                &[Value::Context(ctx.clone()), config.clone()],
            ),
        }
    }
    fn resolve_config(&self, schema: Option<&Value>, config: Value) -> Result<Value, Error> {
        match schema {
            Some(Value::Plugin(plugin)) => plugin.config(config),
            Some(validate @ Value::Function(_)) => {
                self.call(validate, &Value::Undefined, &[config])
            }
            _ => Ok(config),
        }
    }
    fn classify(&self, value: Value) -> EffectValue<Self> {
        match value {
            Value::Undefined | Value::Null => EffectValue::Nullish,
            Value::Function(_) => EffectValue::Dispose(Disposer::Value(value)),
            Value::Effect(effect) => EffectValue::Dispose(Disposer::Effect(effect)),
            Value::Promise(task) => EffectValue::Thenable(Box::pin(task)),
            Value::Array(items) => EffectValue::Iter(Box::new(ArrayIter { items, at: 0 })),
            _ => EffectValue::Invalid,
        }
    }
    fn as_disposer(&self, value: &Value) -> Option<Disposer<Self>> {
        match value {
            Value::Function(_) => Some(Disposer::Value(value.clone())),
            Value::Effect(effect) => Some(Disposer::Effect(effect.clone())),
            _ => None,
        }
    }
    fn check_service(&self, check: &Value, _ctx: &Context, service: &Value) -> Result<bool, Error> {
        Ok(self.call(check, service, &[])?.is_truthy())
    }
    fn event_filter(&self, _this: &Value) -> Option<Value> {
        None
    }
}

impl Default for Context {
    fn default() -> Self {
        Self::new()
    }
}

impl Context {
    /// A new root context.
    pub fn new() -> Self {
        let root = Root::new(NativeHost::default());
        let ctx = Context {
            node: Rc::new(Node {
                parent: None,
                root: root.clone(),
                fiber: Some(root.fiber()),
                isolate: None,
                intercept: None,
            }),
        };
        root.set_ctx(ctx.clone());
        ctx
    }

    fn derive(
        &self,
        fiber: Option<Fiber<NativeHost>>,
        isolate: Option<(String, Key)>,
        intercept: Option<(String, Value)>,
    ) -> Context {
        Context {
            node: Rc::new(Node {
                parent: Some(self.clone()),
                root: self.node.root.clone(),
                fiber,
                isolate,
                intercept,
            }),
        }
    }

    pub fn root(&self) -> &Root<NativeHost> {
        &self.node.root
    }

    pub fn host(&self) -> &NativeHost {
        self.node.root.host()
    }

    /// `ctx.fiber`.
    pub fn fiber(&self) -> Fiber<NativeHost> {
        self.host().fiber_of(self).expect("a context has its fiber")
    }

    /// `ctx.extend()`: a view with nothing of its own.
    pub fn extend(&self) -> Context {
        self.derive(None, None, None)
    }

    /// `ctx.isolate(name, label)`: `name` resolves under `label` (a fresh one when `None`).
    pub fn isolate(&self, name: &str, label: Option<Key>) -> Context {
        let key = label.unwrap_or_else(|| self.host().key());
        self.derive(None, Some((name.to_owned(), key)), None)
    }

    /// `ctx.intercept(name, config)`.
    pub fn intercept(&self, name: &str, config: Value) -> Context {
        self.derive(None, None, Some((name.to_owned(), config)))
    }

    /// The intercept configs for `name`, outermost first.
    pub fn intercepts(&self, name: &str) -> Vec<Value> {
        let mut configs = Vec::new();
        let mut node = Some(self);
        while let Some(current) = node {
            if let Some((n, config)) = &current.node.intercept {
                if n == name {
                    configs.insert(0, config.clone());
                }
            }
            node = current.node.parent.as_ref();
        }
        configs
    }

    /// `ctx.plugin(plugin, config)`.
    pub fn plugin(
        &self,
        plugin: Rc<dyn Plugin>,
        config: Value,
    ) -> Result<Fiber<NativeHost>, Error> {
        let key = Rc::as_ptr(&plugin) as *const () as usize;
        let spec = PluginSpec {
            key,
            name: plugin.name(),
            inject: plugin.inject(),
            schema: Some(Value::Plugin(plugin.clone())),
            callback: Value::Plugin(plugin),
        };
        self.root().plugin(self, spec, config)
    }

    /// A closure as a plugin, with the services it injects.
    pub fn plugin_fn(
        &self,
        name: &str,
        inject: &[&str],
        apply: impl Fn(&Context, &Value) -> Result<Value, Error> + 'static,
    ) -> Result<Fiber<NativeHost>, Error> {
        self.plugin(plugin_fn(Some(name), inject, apply), Value::Undefined)
    }

    /// `ctx.inject(names, callback)`.
    pub fn inject(
        &self,
        names: &[&str],
        apply: impl Fn(&Context, &Value) -> Result<Value, Error> + 'static,
    ) -> Result<Fiber<NativeHost>, Error> {
        self.plugin(plugin_fn(None, names, apply), Value::Undefined)
    }

    /// `ctx.effect(execute, label)`.
    pub fn effect(
        &self,
        label: &str,
        execute: impl Fn() -> Result<Value, Error> + 'static,
    ) -> Result<Effect<NativeHost>, Error> {
        let host_root = self.root().downgrade();
        self.fiber().effect(
            Rc::new(move || {
                let value = execute()?;
                let root = host_root.upgrade().map(crate::root::Root::from_inner);
                Ok(match root {
                    Some(root) => root.host().classify(value),
                    None => EffectValue::Nullish,
                })
            }),
            label,
        )
    }

    /// `ctx.provide(name, value)`.
    pub fn provide(&self, name: &str, value: Value) -> Result<Effect<NativeHost>, Error> {
        self.root().provide(self, name, value, None)
    }

    /// `ctx.provide(name, value, check)`.
    pub fn provide_checked(
        &self,
        name: &str,
        value: Value,
        check: impl Fn(&Value) -> bool + 'static,
    ) -> Result<Effect<NativeHost>, Error> {
        let check = Value::function(move |this, _| Ok(Value::Bool(check(this))));
        self.root().provide(self, name, value, Some(check))
    }

    /// `ctx[name]`: an accessor, else the service this context reaches.
    pub fn get(&self, name: &str) -> Result<Option<Value>, Error> {
        if let Some(crate::root::Prop::Accessor { get: Some(get), .. }) = self.root().prop(name) {
            return get
                .call(self.host(), &Value::Context(self.clone()), &[])
                .map(Some);
        }
        match self.root().lookup(&self.fiber(), name) {
            Lookup::Found(value) => Ok(Some(value)),
            Lookup::Missing => Ok(None),
            Lookup::Inactive => Err(Error::new(format!(
                "cannot get required service \"{name}\" in inactive context"
            ))),
        }
    }

    /// `ctx[name]`, as a plugin's own type.
    pub fn get_as<T: Any>(&self, name: &str) -> Option<Rc<T>> {
        self.get(name)
            .ok()
            .flatten()
            .and_then(|v| v.downcast::<T>())
    }

    /// `ctx.reflect.set(name, value)`.
    pub fn set(&self, name: &str, value: Value) -> Result<(), Error> {
        self.root().set_service(self, name, value)
    }

    /// `ctx.accessor(name, { get })`.
    pub fn accessor(
        &self,
        name: &str,
        get: impl Fn(&Context) -> Result<Value, Error> + 'static,
    ) -> Result<Effect<NativeHost>, Error> {
        let get: NativeFn<NativeHost> = Rc::new(move |this, _| match this {
            Value::Context(ctx) => get(ctx),
            _ => Ok(Value::Undefined),
        });
        self.root()
            .accessor(self, name, Some(Callback::Native(get)), None)
    }

    /// `ctx.on(name, listener)`.
    pub fn on(
        &self,
        name: &str,
        listener: impl Fn(&[Value]) -> Result<Value, Error> + 'static,
    ) -> Result<Disposer<NativeHost>, Error> {
        self.on_with(name, listener, OnOptions::default())
    }

    pub fn on_with(
        &self,
        name: &str,
        listener: impl Fn(&[Value]) -> Result<Value, Error> + 'static,
        options: OnOptions<NativeHost>,
    ) -> Result<Disposer<NativeHost>, Error> {
        let callback: Callback<NativeHost> =
            Callback::Value(Value::function(move |_, args| listener(args)));
        self.root().on(self, name, callback, options)
    }

    pub fn emit(&self, name: &str, args: &[Value]) -> Result<(), Error> {
        self.root().emit(None, name, args)
    }

    /// `ctx.emit(this, name, ...)`, with a filter on the listening contexts.
    pub fn emit_filtered(
        &self,
        name: &str,
        args: &[Value],
        filter: impl Fn(&Context) -> bool + 'static,
    ) -> Result<(), Error> {
        self.root()
            .emit_filtered(None, name, args, &Filter::Native(Rc::new(filter)))
    }

    pub fn bail(&self, name: &str, args: &[Value]) -> Result<Value, Error> {
        self.root().bail(None, name, args)
    }

    pub fn serial(&self, name: &str, args: Vec<Value>) -> Task<Result<Value, Error>> {
        self.root().serial(None, name, args)
    }

    pub fn parallel(&self, name: &str, args: Vec<Value>) -> Task<Result<(), Error>> {
        self.root().parallel(None, name, args)
    }

    pub fn waterfall(
        &self,
        name: &str,
        args: Vec<Value>,
        inner: impl Fn(&[Value]) -> Result<Value, Error> + 'static,
    ) -> Result<Value, Error> {
        let inner: NativeFn<NativeHost> = Rc::new(move |_, args| inner(args));
        self.root()
            .waterfall(None, name, args, Callback::Native(inner))
    }
}
