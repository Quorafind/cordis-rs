//! Rust plugins in a JavaScript context tree.
//!
//! A [`RustPlugin`] becomes a JavaScript plugin object — `{ name, inject, apply }`, `apply`
//! a native function — so `ctx.plugin(...)` registers it in the same registry as JS plugins: it
//! gets a fiber, waits for what it injects, is reloaded when a provider changes, and its effects
//! are undone when it is disposed. Its body gets a [`Ctx`], the Rust face of the context it was
//! applied on, which drives the same engine the JS face does.
//!
//! [`define_plugin`] also makes it importable: `import greeter from 'cordis-rust:greeter'`, a
//! module whose own `apply` makes compose-runtime's loader treat it as a Cordis plugin.

use std::rc::Rc;

use boa_engine::object::JsObject;
use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsString, JsValue, js_string};
use cordis::host::{EffectValue, Host};
use cordis::{Callback, Disposer, Effect, Fiber, OnOptions, Root};

use crate::cx;
use crate::host::{JsHost, RootHandle, make_fn};

/// A plugin written in Rust, for a JavaScript tree.
pub trait RustPlugin: 'static {
    /// Its name: `fiber.name`, and what `ctx.logger()` is named after.
    fn name(&self) -> &str;

    /// The services it injects; it loads once all are there, and reloads when one changes.
    fn inject(&self) -> Vec<String> {
        Vec::new()
    }

    /// Its body. What it answers is collected as an effect body's answer would be: a JS
    /// function (a disposer), `undefined`, a promise.
    fn apply(&self, ctx: &Ctx, config: &JsValue) -> JsResult<JsValue>;
}

/// A JavaScript context, from Rust.
#[derive(Clone)]
pub struct Ctx {
    ctx: JsObject,
    root: Root<JsHost>,
}

fn root_of(ctx: &JsObject) -> JsResult<Root<JsHost>> {
    let statics = cx::with(|context| context.get_data::<Rc<crate::host::Statics>>().cloned())
        .ok_or_else(|| JsNativeError::typ().with_message("cordis-verve is not installed"))?;
    cx::with(|context| {
        let handle = ctx.get(statics.root.clone(), context)?;
        handle
            .as_object()
            .and_then(|object| object.downcast_ref::<RootHandle>().map(|h| h.0.clone()))
            .ok_or_else(|| {
                JsNativeError::typ()
                    .with_message("not a cordis context")
                    .into()
            })
    })
}

impl Ctx {
    /// The Rust face of a JS context object.
    pub fn from_js(ctx: JsObject) -> JsResult<Self> {
        let root = root_of(&ctx)?;
        Ok(Self { ctx, root })
    }

    /// The JS context object.
    pub fn js(&self) -> &JsObject {
        &self.ctx
    }

    /// The engine's root of this tree.
    pub fn root(&self) -> &Root<JsHost> {
        &self.root
    }

    /// `ctx.fiber`.
    pub fn fiber(&self) -> Fiber<JsHost> {
        self.root
            .host()
            .fiber_of(&self.ctx)
            .expect("a context has its fiber")
    }

    /// `ctx[name]`, through the context's own property semantics (accessors, injection).
    pub fn get(&self, name: &str) -> JsResult<JsValue> {
        cx::with(|context| self.ctx.get(JsString::from(name), context))
    }

    /// `ctx.on(name, listener)`: a listener of this context's fiber, undone with it.
    pub fn on(
        &self,
        name: &str,
        listener: impl Fn(&JsValue, &[JsValue]) -> JsResult<JsValue> + 'static,
    ) -> JsResult<Disposer<JsHost>> {
        self.root.on(
            &self.ctx,
            name,
            Callback::Native(Rc::new(listener)),
            OnOptions::default(),
        )
    }

    /// `ctx.emit(name, ...args)`.
    pub fn emit(&self, name: &str, args: &[JsValue]) -> JsResult<()> {
        self.root.emit(None, name, args)
    }

    /// `ctx.provide(name, value)`: a service of this context's fiber.
    pub fn provide(&self, name: &str, value: JsValue) -> JsResult<Effect<JsHost>> {
        self.root.provide(&self.ctx, name, value, None)
    }

    /// `ctx.effect(execute, label)`: `execute` answers what an effect body answers.
    pub fn effect(
        &self,
        label: &str,
        execute: impl Fn() -> JsResult<JsValue> + 'static,
    ) -> JsResult<Effect<JsHost>> {
        let root = self.root.downgrade();
        self.fiber().effect(
            Rc::new(move || {
                let value = execute()?;
                Ok(match root.upgrade() {
                    Some(root) => root.host.classify(value),
                    None => EffectValue::Nullish,
                })
            }),
            label,
        )
    }

    /// A function value running `f`, for a service's methods or a disposer.
    pub fn function(
        &self,
        name: &str,
        f: impl Fn(&JsValue, &[JsValue]) -> JsResult<JsValue> + 'static,
    ) -> JsValue {
        cx::with(|context| make_fn(context, name, 0, f).into())
    }

    /// A plain object with these properties.
    pub fn object(&self, properties: &[(&str, JsValue)]) -> JsResult<JsValue> {
        cx::with(|context| {
            let object = JsObject::with_object_proto(context.intrinsics());
            for (key, value) in properties {
                object.set(JsString::from(*key), value.clone(), false, context)?;
            }
            Ok(object.into())
        })
    }
}

/// The JS plugin object for `plugin`: `{ name, inject, apply }`.
pub fn plugin_object(context: &mut Context, plugin: Rc<dyn RustPlugin>) -> JsResult<JsObject> {
    let object = JsObject::with_object_proto(context.intrinsics());
    let name = JsString::from(plugin.name());
    let inject: Vec<JsValue> = plugin
        .inject()
        .into_iter()
        .map(|n| JsString::from(n).into())
        .collect();
    let inject = JsArray::from_iter(inject, context);
    let body = plugin.clone();
    let apply = make_fn(context, plugin.name(), 2, move |_, args| {
        let ctx = args
            .first()
            .and_then(JsValue::as_object)
            .ok_or_else(|| JsError::from(JsNativeError::typ().with_message("no context")))?;
        let config = args.get(1).cloned().unwrap_or_default();
        body.apply(&Ctx::from_js(ctx)?, &config)
    });
    object.set(js_string!("name"), name, false, context)?;
    object.set(js_string!("inject"), inject, false, context)?;
    object.set(js_string!("apply"), apply, false, context)?;
    Ok(object)
}

/// Makes `plugin` importable as `specifier` (`import plugin from 'cordis-rust:x'`, or
/// `require`). The module's own `apply`, `name` and `inject` are the plugin's, and its default
/// export the plugin object, so either shape a loader looks for finds it.
pub fn define_plugin(
    runtime: &mut verve::Runtime,
    specifier: &str,
    plugin: Rc<dyn RustPlugin>,
) -> Result<JsValue, String> {
    crate::install(runtime)?;
    let exports = {
        let context = runtime.context();
        let made = cx::enter(context, || -> JsResult<JsObject> {
            let object = cx::with(|context| plugin_object(context, plugin))?;
            cx::with(|context| -> JsResult<JsObject> {
                let exports = JsObject::with_null_proto();
                for key in ["name", "inject", "apply"] {
                    let value = object.get(JsString::from(key), context)?;
                    exports.set(JsString::from(key), value, false, context)?;
                }
                exports.set(js_string!("default"), object, false, context)?;
                Ok(exports)
            })
        });
        made.map_err(|error| error.to_string())?
    };
    let value = JsValue::from(exports);
    runtime.define_builtin(specifier, value.clone());
    Ok(value)
}
