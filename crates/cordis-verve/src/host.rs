//! The engine's host for JavaScript: values are `JsValue`s, contexts the JS context objects,
//! isolation keys their symbols.
//!
//! Every method reaches Boa through [`cx::with`]; none calls back into the engine from inside
//! that closure (only through JavaScript), which is what keeps the recorded context sound.

use std::cell::RefCell;
use std::collections::HashMap;
use std::rc::Rc;

use boa_engine::object::builtins::{JsArray, JsPromise};
use boa_engine::object::{FunctionObjectBuilder, JsObject};
use boa_engine::{
    Context, JsError, JsNativeError, JsResult, JsString, JsSymbol, JsValue, NativeFunction,
    js_string,
};
use boa_gc::{Finalize, Trace};
use cordis::host::{AsyncEffectIter, EffectIter, EffectValue, ErrorKind, Host, NativeFn};
use cordis::task::{Deferred, LocalBoxFuture, Scope, Task};
use cordis::{Disposer, Effect, Fiber, Runtime};

use crate::cx;
use crate::sweep::SweepSafe;

/// The private symbols the engine and `cordis.js` share, and what is fetched once.
pub(crate) struct Statics {
    /// A face's handle on its native object.
    pub(crate) handle: JsSymbol,
    /// A context's root handle.
    pub(crate) root: JsSymbol,
    /// A fiber face's context.
    pub(crate) ctx: JsSymbol,
    /// A fiber face's parent context.
    pub(crate) parent: JsSymbol,
    pub(crate) isolate: JsSymbol,
    pub(crate) filter: JsSymbol,
    /// `AsyncLocalStorage.snapshot`, when the runtime has it.
    pub(crate) snapshot: Option<JsObject>,
}

/// An isolation label: a symbol, as `ctx.isolate()` makes them; anything else by its text.
#[derive(Clone, Debug, PartialEq, Eq, Hash)]
pub enum JsKey {
    Symbol(JsSymbol),
    Text(JsString),
}

impl JsKey {
    fn of(value: &JsValue, context: &mut Context) -> Option<Self> {
        if value.is_undefined() {
            return None;
        }
        if let Some(symbol) = value.as_symbol() {
            return Some(Self::Symbol(symbol));
        }
        value.to_string(context).ok().map(Self::Text)
    }
}

/// Native objects carried by faces. What they hold is dropped when the collector frees them,
/// so it is sweep-safe (see [`crate::sweep`]).
macro_rules! handle {
    ($name:ident, $inner:ty) => {
        #[derive(Trace, Finalize, boa_engine::JsData)]
        pub(crate) struct $name(#[unsafe_ignore_trace] pub(crate) SweepSafe<$inner>);

        impl $name {
            pub(crate) fn new(value: $inner) -> Self {
                Self(SweepSafe::new(value))
            }
        }
    };
}

handle!(RootHandle, cordis::Root<JsHost>);
handle!(FiberHandle, Fiber<JsHost>);
handle!(EffectHandle, Effect<JsHost>);
handle!(RuntimeHandle, Runtime<JsHost>);

/// The host of one context tree.
pub struct JsHost {
    pub(crate) statics: Rc<Statics>,
    /// What `cordis.js` makes for the engine (faces, contexts, errors).
    pub(crate) helpers: JsObject,
    /// The root context (the proxy), for the root's isolation keys.
    pub(crate) root_ctx: RefCell<Option<JsObject>>,
    /// Symbol event names, by the text the engine knows them by.
    pub(crate) names: RefCell<HashMap<String, JsSymbol>>,
}

/// A native function value running `f` inside [`cx::enter`].
pub(crate) fn make_fn(
    context: &mut Context,
    name: &str,
    length: usize,
    f: impl Fn(&JsValue, &[JsValue]) -> JsResult<JsValue> + 'static,
) -> JsObject {
    // The captures are dropped when the function object is freed, during a sweep: sweep-safe.
    let f = SweepSafe::new(f);
    // SAFETY: the closure captures only Rust state and rooted `JsValue`s; Boa's collector
    // treats a value held outside its heap as a root, so nothing it captures can be freed
    // while it is alive. (A cycle through it is never collected; the engine cuts those.)
    let native = unsafe {
        NativeFunction::from_closure(move |this, args, context| {
            cx::enter(context, || (*f)(this, args))
        })
    };
    FunctionObjectBuilder::new(context.realm(), native)
        .name(JsString::from(name))
        .length(length)
        .build()
        .into()
}

pub(crate) fn get(object: &JsObject, key: &str, context: &mut Context) -> JsResult<JsValue> {
    object.get(JsString::from(key), context)
}

fn call_helper(
    helpers: &JsObject,
    name: &str,
    args: &[JsValue],
    context: &mut Context,
) -> JsResult<JsValue> {
    let f = get(helpers, name, context)?;
    let f = f
        .as_callable()
        .ok_or_else(|| JsNativeError::typ().with_message(format!("cordis.js has no {name}")))?;
    f.call(&JsValue::undefined(), args, context)
}

/// The native object behind a face (or the handle itself).
pub(crate) fn handle_object(
    value: &JsValue,
    statics: &Statics,
    context: &mut Context,
) -> Option<JsObject> {
    let object = value.as_object()?;
    if object.downcast_ref::<FiberHandle>().is_some()
        || object.downcast_ref::<EffectHandle>().is_some()
        || object.downcast_ref::<RuntimeHandle>().is_some()
        || object.downcast_ref::<RootHandle>().is_some()
    {
        return Some(object.clone());
    }
    object
        .get(statics.handle.clone(), context)
        .ok()?
        .as_object()
}

pub(crate) fn fiber_of_value(
    value: &JsValue,
    statics: &Statics,
    context: &mut Context,
) -> Option<Fiber<JsHost>> {
    let handle = handle_object(value, statics, context)?;
    handle.downcast_ref::<FiberHandle>().map(|h| h.0.clone())
}

pub(crate) fn effect_of_value(
    value: &JsValue,
    statics: &Statics,
    context: &mut Context,
) -> Option<Effect<JsHost>> {
    if !value.is_callable() {
        return None;
    }
    let handle = handle_object(value, statics, context)?;
    handle.downcast_ref::<EffectHandle>().map(|h| h.0.clone())
}

fn opaque(error: JsError, context: &mut Context) -> JsValue {
    error
        .into_opaque(context)
        .unwrap_or_else(|e| JsValue::from(JsString::from(e.to_string())))
}

/// An `AsyncLocalStorage` snapshot: each poll of a task runs inside the stores its spawner had.
struct JsScope {
    run: JsObject,
}

thread_local! {
    /// The poll a scope is running, for the native callback it hands the snapshot.
    static POLL: RefCell<Vec<*mut (dyn FnMut() + 'static)>> = const { RefCell::new(Vec::new()) };
}

impl Scope for JsScope {
    fn enter(&self, f: &mut dyn FnMut()) {
        // SAFETY: the pointer is popped before `enter` returns, and only the callback below —
        // called synchronously by the snapshot function — dereferences it.
        let erased: *mut (dyn FnMut() + '_) = f;
        let erased: *mut (dyn FnMut() + 'static) = unsafe { std::mem::transmute(erased) };
        POLL.with(|stack| stack.borrow_mut().push(erased));
        let ran = cx::try_with(|context| {
            let callback = make_fn(context, "poll", 0, |_, _| {
                let top = POLL.with(|stack| stack.borrow().last().copied());
                if let Some(top) = top {
                    // SAFETY: see above; the frame owning `f` is waiting on this call.
                    unsafe { (*top)() };
                }
                Ok(JsValue::undefined())
            });
            self.run
                .call(&JsValue::undefined(), &[callback.into()], context)
                .is_ok()
        });
        let popped = POLL.with(|stack| stack.borrow_mut().pop());
        if ran != Some(true) {
            // No engine to run it in, or the snapshot threw: poll in place.
            if let Some(top) = popped {
                unsafe { (*top)() };
            }
        }
    }
}

struct JsIter {
    iterator: JsObject,
}

fn step(result: &JsValue, context: &mut Context) -> JsResult<(JsValue, bool)> {
    let object = result
        .as_object()
        .ok_or_else(|| JsNativeError::typ().with_message("Iterator result is not an object"))?;
    let done = object.get(js_string!("done"), context)?.to_boolean();
    let value = object.get(js_string!("value"), context)?;
    Ok((value, done))
}

impl EffectIter<JsHost> for JsIter {
    fn next(&mut self) -> Result<(JsValue, bool), JsError> {
        cx::with(|context| {
            let next = self.iterator.get(js_string!("next"), context)?;
            let next = next.as_callable().ok_or_else(|| {
                JsNativeError::typ().with_message("iterator.next is not a function")
            })?;
            let result = next.call(&self.iterator.clone().into(), &[], context)?;
            step(&result, context)
        })
    }
}

struct JsAsyncIter {
    iterator: JsObject,
    host: Rc<Statics>,
}

impl AsyncEffectIter<JsHost> for JsAsyncIter {
    fn next(&mut self) -> LocalBoxFuture<Result<(JsValue, bool), JsError>> {
        let called = cx::with(|context| {
            let next = self.iterator.get(js_string!("next"), context)?;
            let next = next.as_callable().ok_or_else(|| {
                JsNativeError::typ().with_message("iterator.next is not a function")
            })?;
            next.call(&self.iterator.clone().into(), &[], context)
        });
        let _ = &self.host;
        Box::pin(async move {
            let result = await_js(called?).await?;
            cx::with(|context| step(&result, context))
        })
    }
}

/// `await value`: `Promise.resolve(value).then(...)`, its settlement as a future.
pub(crate) fn await_js(value: JsValue) -> LocalBoxFuture<Result<JsValue, JsError>> {
    let settled = Deferred::<Result<JsValue, JsError>>::new();
    let subscribed = cx::with(|context| -> JsResult<()> {
        let promise = JsPromise::resolve(value, context)?;
        let ok = settled.clone();
        let on_fulfilled = make_fn(context, "", 1, move |_, args| {
            ok.settle(Ok(args.first().cloned().unwrap_or_default()));
            Ok(JsValue::undefined())
        });
        let err = settled.clone();
        let on_rejected = make_fn(context, "", 1, move |_, args| {
            err.settle(Err(JsError::from_opaque(
                args.first().cloned().unwrap_or_default(),
            )));
            Ok(JsValue::undefined())
        });
        promise.then(
            Some(
                boa_engine::object::builtins::JsFunction::from_object(on_fulfilled)
                    .expect("callable"),
            ),
            Some(
                boa_engine::object::builtins::JsFunction::from_object(on_rejected)
                    .expect("callable"),
            ),
            context,
        )?;
        Ok(())
    });
    if let Err(error) = subscribed {
        settled.settle(Err(error));
    }
    Box::pin(settled.wait())
}

/// A task as a JavaScript promise.
pub(crate) fn promise_of(task: Task<Result<JsValue, JsError>>) -> JsValue {
    let (promise, resolvers) = cx::with(JsPromise::new_pending);
    let value: JsValue = promise.into();
    cordis::task::spawn(async move {
        let outcome = task.await;
        cx::with(|context| {
            let _ = match outcome {
                Ok(value) => resolvers
                    .resolve
                    .call(&JsValue::undefined(), &[value], context),
                Err(error) => {
                    let reason = opaque(error, context);
                    resolvers
                        .reject
                        .call(&JsValue::undefined(), &[reason], context)
                }
            };
        });
    });
    value
}

impl JsHost {
    fn helper(&self, name: &str, args: &[JsValue]) -> JsResult<JsValue> {
        cx::with(|context| call_helper(&self.helpers, name, args, context))
    }

    fn runtime_face(&self, runtime: &Runtime<JsHost>) -> JsValue {
        if let Some(face) = runtime.face() {
            return face;
        }
        let callback = runtime.callback().clone();
        let schema = runtime.schema().cloned().unwrap_or_default();
        let handle = JsObject::from_proto_and_data(None, RuntimeHandle::new(runtime.clone()));
        let face = self
            .helper("makeRuntime", &[handle.into(), callback, schema])
            .unwrap_or_default();
        runtime.set_face(face.clone());
        face
    }

    pub(crate) fn runtime_value(&self, runtime: &Runtime<JsHost>) -> JsValue {
        self.runtime_face(runtime)
    }

    /// The engine's text for an event name: a string as itself, a symbol by its identity.
    pub(crate) fn name_key(&self, name: &JsValue, context: &mut Context) -> JsResult<String> {
        if let Some(symbol) = name.as_symbol() {
            let key = format!("\u{0}symbol:{}", symbol.hash());
            self.names.borrow_mut().insert(key.clone(), symbol);
            return Ok(key);
        }
        Ok(name.to_string(context)?.to_std_string_escaped())
    }
}

impl Host for JsHost {
    type Value = JsValue;
    type Ctx = JsObject;
    type Key = JsKey;
    type Error = JsError;
    type PluginKey = JsObject;

    fn undefined(&self) -> JsValue {
        JsValue::undefined()
    }
    fn null(&self) -> JsValue {
        JsValue::null()
    }
    fn string(&self, text: &str) -> JsValue {
        JsString::from(text).into()
    }
    fn number(&self, number: f64) -> JsValue {
        JsValue::from(number)
    }
    fn boolean(&self, value: bool) -> JsValue {
        JsValue::from(value)
    }
    fn array(&self, items: Vec<JsValue>) -> JsValue {
        cx::with(|context| JsArray::from_iter(items, context).into())
    }
    fn is_nullish(&self, value: &JsValue) -> bool {
        value.is_null_or_undefined()
    }
    fn is_bailed(&self, value: &JsValue) -> bool {
        !(value.is_null_or_undefined() || value.as_boolean() == Some(false))
    }
    fn is_truthy(&self, value: &JsValue) -> bool {
        value.to_boolean()
    }
    fn same(&self, a: &JsValue, b: &JsValue) -> bool {
        a.strict_equals(b)
    }
    fn fiber_value(&self, fiber: &Fiber<Self>) -> JsValue {
        if let Some(face) = fiber.face() {
            return face;
        }
        let handle = JsObject::from_proto_and_data(None, FiberHandle::new(fiber.clone()));
        let face = self
            .helper("makeFiber", &[handle.into()])
            .unwrap_or_default();
        // Kept while the fiber lives; a disposed fiber's face is not held (it would hold the
        // fiber back through its own handle).
        if fiber.uid().is_some() || fiber.is_root() {
            fiber.set_face(Some(face.clone()));
        }
        face
    }
    fn fiber_from_value(&self, value: &JsValue) -> Option<Fiber<Self>> {
        cx::with(|context| fiber_of_value(value, &self.statics, context))
    }
    fn effect_value(&self, effect: &Effect<Self>) -> JsValue {
        if let Some(face) = effect.face() {
            return face;
        }
        let handle = JsObject::from_proto_and_data(None, EffectHandle::new(effect.clone()));
        let face = self
            .helper("makeEffect", &[handle.into()])
            .unwrap_or_default();
        if effect.is_active() {
            effect.set_face(Some(face.clone()));
        }
        face
    }
    fn ctx_value(&self, ctx: &JsObject) -> JsValue {
        ctx.clone().into()
    }
    fn native_function(&self, name: &str, f: NativeFn<Self>) -> JsValue {
        cx::with(|context| make_fn(context, name, 0, move |this, args| f(this, args)).into())
    }
    fn event_name(&self, name: &str) -> JsValue {
        match self.names.borrow().get(name) {
            Some(symbol) => symbol.clone().into(),
            None => JsString::from(name).into(),
        }
    }
    fn call(&self, f: &JsValue, this: &JsValue, args: &[JsValue]) -> Result<JsValue, JsError> {
        cx::with(|context| {
            let callable = f
                .as_callable()
                .ok_or_else(|| JsNativeError::typ().with_message("not a function"))?;
            callable.call(this, args, context)
        })
    }
    fn is_thenable(&self, value: &JsValue) -> bool {
        let Some(object) = value.as_object() else {
            return false;
        };
        cx::with(|context| {
            object
                .has_property(js_string!("then"), context)
                .unwrap_or(false)
        })
    }
    fn await_value(&self, value: JsValue) -> LocalBoxFuture<Result<JsValue, JsError>> {
        await_js(value)
    }
    fn promise(&self, task: Task<Result<JsValue, JsError>>) -> JsValue {
        promise_of(task)
    }
    fn scope(&self) -> Option<Rc<dyn Scope>> {
        let snapshot = self.statics.snapshot.clone()?;
        let run = cx::try_with(|context| snapshot.call(&JsValue::undefined(), &[], context).ok())??;
        let run = run.as_object()?.clone();
        Some(Rc::new(JsScope { run }))
    }
    fn error(&self, kind: ErrorKind, message: &str) -> JsError {
        let kind = match kind {
            ErrorKind::Error => "Error",
            ErrorKind::TypeError => "TypeError",
            ErrorKind::Cordis(code) => code,
        };
        match self.helper(
            "makeError",
            &[JsString::from(kind).into(), JsString::from(message).into()],
        ) {
            Ok(value) => JsError::from_opaque(value),
            Err(error) => error,
        }
    }
    fn aggregate_error(&self, errors: Vec<JsError>) -> JsError {
        let values = cx::with(|context| {
            let values: Vec<JsValue> = errors.into_iter().map(|e| opaque(e, context)).collect();
            JsValue::from(JsArray::from_iter(values, context))
        });
        match self.helper("aggregate", &[values]) {
            Ok(value) => JsError::from_opaque(value),
            Err(error) => error,
        }
    }
    fn compose_error(&self, error: JsError) -> JsError {
        let value = cx::with(|context| opaque(error, context));
        match self.helper("composeError", &[value]) {
            Ok(value) => JsError::from_opaque(value),
            Err(error) => error,
        }
    }
    fn report(&self, ctx: &JsObject, error: &JsError) {
        let value = cx::with(|context| opaque(error.clone(), context));
        if let Err(failure) = self.helper("report", &[ctx.clone().into(), value]) {
            cx::with(|context| {
                let text = failure.to_string();
                verve::console_line(context, verve::Level::Error, format!("cordis: {text}"));
            });
        }
    }
    fn fiber_of(&self, ctx: &JsObject) -> Option<Fiber<Self>> {
        cx::with(|context| {
            let face = ctx.get(js_string!("fiber"), context).ok()?;
            fiber_of_value(&face, &self.statics, context)
        })
    }
    fn derive_fiber_ctx(
        &self,
        parent: &JsObject,
        fiber: &Fiber<Self>,
        inject: &[(String, JsValue)],
        has_inject: bool,
    ) -> JsObject {
        let face = self.fiber_value(fiber);
        let entries = cx::with(|context| {
            let entries: Vec<JsValue> = inject
                .iter()
                .map(|(name, config)| {
                    JsArray::from_iter(
                        [JsString::from(name.as_str()).into(), config.clone()],
                        context,
                    )
                    .into()
                })
                .collect();
            JsValue::from(JsArray::from_iter(entries, context))
        });
        let ctx = self
            .helper(
                "deriveFiberCtx",
                &[parent.clone().into(), face, entries, has_inject.into()],
            )
            .ok()
            .and_then(|value| value.as_object());
        ctx.unwrap_or_else(|| parent.clone())
    }
    fn isolate_key(&self, ctx: &JsObject, name: &str) -> Option<JsKey> {
        cx::with(|context| {
            let map = ctx.get(self.statics.isolate.clone(), context).ok()?;
            let map = map.as_object()?;
            let key = map.get(JsString::from(name), context).ok()?;
            JsKey::of(&key, context)
        })
    }
    fn root_key(&self, name: &str) -> Option<JsKey> {
        let root = self.root_ctx.borrow().clone()?;
        self.isolate_key(&root, name)
    }
    fn ensure_root_key(&self, name: &str) -> JsKey {
        if let Some(key) = self.root_key(name) {
            return key;
        }
        let symbol = JsSymbol::new(Some(JsString::from(name))).expect("a symbol");
        if let Some(root) = self.root_ctx.borrow().clone() {
            cx::with(|context| {
                if let Ok(map) = root.get(self.statics.isolate.clone(), context)
                    && let Some(map) = map.as_object()
                {
                    let _ = map.set(JsString::from(name), symbol.clone(), false, context);
                }
            });
        }
        JsKey::Symbol(symbol)
    }
    fn execute_plugin(
        &self,
        runtime: &Runtime<Self>,
        callback: &JsValue,
        ctx: &JsObject,
        config: &JsValue,
    ) -> Result<JsValue, JsError> {
        let face = self.runtime_face(runtime);
        self.helper(
            "executePlugin",
            &[face, callback.clone(), ctx.clone().into(), config.clone()],
        )
    }
    fn resolve_config(
        &self,
        schema: Option<&JsValue>,
        config: JsValue,
    ) -> Result<JsValue, JsError> {
        match schema {
            Some(schema) if !schema.is_null_or_undefined() => {
                self.helper("resolveConfig", &[schema.clone(), config])
            }
            _ => Ok(config),
        }
    }
    fn classify(&self, value: JsValue) -> EffectValue<Self> {
        if value.is_callable() {
            return match self.as_disposer(&value) {
                Some(disposer) => EffectValue::Dispose(disposer),
                None => EffectValue::Invalid,
            };
        }
        if value.is_null_or_undefined() {
            return EffectValue::Nullish;
        }
        if !value.is_object() {
            return EffectValue::Invalid;
        }
        if self.is_thenable(&value) {
            return EffectValue::Thenable(await_js(value));
        }
        let iterated = self.helper("iterate", &[value]);
        let Ok(iterated) = iterated else {
            return EffectValue::Invalid;
        };
        cx::with(|context| {
            let Some(pair) = iterated.as_object() else {
                return EffectValue::Invalid;
            };
            let kind = pair
                .get(0, context)
                .ok()
                .and_then(|k| k.as_number())
                .unwrap_or(0.);
            let iterator = pair.get(1, context).ok().and_then(|i| i.as_object());
            match (kind as u8, iterator) {
                (1, Some(iterator)) => EffectValue::Iter(Box::new(JsIter { iterator })),
                (2, Some(iterator)) => EffectValue::AsyncIter(Box::new(JsAsyncIter {
                    iterator,
                    host: self.statics.clone(),
                })),
                _ => EffectValue::Invalid,
            }
        })
    }
    fn as_disposer(&self, value: &JsValue) -> Option<Disposer<Self>> {
        if !value.is_callable() {
            return None;
        }
        let effect = cx::with(|context| effect_of_value(value, &self.statics, context));
        Some(match effect {
            Some(effect) => Disposer::Effect(effect),
            None => Disposer::Value(value.clone()),
        })
    }
    fn check_service(
        &self,
        check: &JsValue,
        ctx: &JsObject,
        service: &JsValue,
    ) -> Result<bool, JsError> {
        let answer = self.helper(
            "checkService",
            &[check.clone(), ctx.clone().into(), service.clone()],
        )?;
        Ok(answer.to_boolean())
    }
    fn event_filter(&self, this: &JsValue) -> Option<JsValue> {
        let object = this.as_object()?;
        cx::with(|context| {
            let filter = object.get(self.statics.filter.clone(), context).ok()?;
            filter.is_callable().then_some(filter)
        })
    }
}
