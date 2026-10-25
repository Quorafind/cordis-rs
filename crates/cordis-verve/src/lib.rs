//! The native Cordis on Verve: the `cordis` module, backed by the Rust engine.
//!
//! [`install`] defines `cordis` as a builtin module of a [`verve::Runtime`], so
//! `import { Context, Service } from 'cordis'` (and `require('cordis')`) answer the native
//! implementation. JavaScript plugins, services and listeners registered through it live in the
//! engine's registry beside Rust ones.
//!
//! The JavaScript face (`js/cordis.js`) is Cordis 4.0.0-rc.8's object model — the `Context`
//! proxy, prototype chains, traceable services, `Service`, `Logger` — over native state:
//! fibers, effects, events, services and the registry are the engine's (`cordis`), reached
//! through the ops in [`ops`].

mod cx;
mod host;
mod ops;
mod rust;

use std::rc::Rc;

use boa_engine::job::{Job, PromiseJob};
use boa_engine::object::JsObject;
use boa_engine::{Context, JsResult, JsString, JsSymbol, JsValue, Source, js_string};

pub use host::{JsHost, JsKey};
pub use rust::{Ctx, RustPlugin, define_plugin, plugin_object};

use crate::host::Statics;

/// The JavaScript face, evaluated once per realm.
const CORDIS_JS: &str = include_str!("../js/cordis.js");

/// The specifiers [`install`] defines.
pub const MODULE_NAMES: &[&str] = &["cordis"];

/// Defines the `cordis` builtin module on `runtime`, backed by the native engine. Answers the
/// module's exports.
pub fn install(runtime: &mut verve::Runtime) -> Result<JsValue, String> {
    let exports = {
        let context = runtime.context();
        exports(context).map_err(|error| error.to_string())?
    };
    for name in MODULE_NAMES {
        runtime.define_builtin(name, exports.clone());
    }
    Ok(exports)
}

/// The exports of the `cordis` module for `context`, made once per realm.
pub fn exports(context: &mut Context) -> JsResult<JsValue> {
    if let Some(exports) = context.get_data::<Exports>() {
        return Ok(exports.0.clone());
    }
    build(context)
}

#[derive(Clone)]
struct Exports(JsValue);

fn symbol(description: &str) -> JsSymbol {
    JsSymbol::new(Some(JsString::from(description))).expect("a symbol")
}

fn registered(context: &mut Context, key: &str) -> JsResult<JsSymbol> {
    let value = context.eval(Source::from_bytes(&format!("Symbol.for({key:?})")))?;
    Ok(value.as_symbol().expect("Symbol.for answers a symbol"))
}

fn snapshot(context: &mut Context) -> Option<JsObject> {
    let hooks = verve::require(context, "async_hooks").ok()?;
    let storage = hooks
        .as_object()?
        .get(js_string!("AsyncLocalStorage"), context)
        .ok()?;
    let snapshot = storage
        .as_object()?
        .get(js_string!("snapshot"), context)
        .ok()?;
    let snapshot = snapshot.as_object()?.clone();
    // Bound to the class, as `AsyncLocalStorage.snapshot()` is called.
    let bind = snapshot.get(js_string!("bind"), context).ok()?;
    let bound = bind
        .as_callable()?
        .call(&snapshot.clone().into(), &[storage], context)
        .ok()?;
    bound.as_object()
}

fn build(context: &mut Context) -> JsResult<JsValue> {
    let statics = Rc::new(Statics {
        handle: symbol("cordis.native.handle"),
        root: symbol("cordis.native.root"),
        ctx: symbol("cordis.native.ctx"),
        parent: symbol("cordis.native.parent"),
        isolate: registered(context, "cordis.isolate")?,
        filter: registered(context, "cordis.filter")?,
        snapshot: snapshot(context),
    });
    context.insert_data(statics.clone());
    install_scheduler();
    let exports = cx::enter(context, || -> JsResult<JsValue> {
        let (factory, ops, keys) = cx::with(|context| -> JsResult<_> {
            let factory = context.eval(Source::from_bytes(CORDIS_JS))?;
            let ops = ops::ops(context)?;
            let keys = JsObject::with_null_proto();
            keys.set(js_string!("handle"), statics.handle.clone(), false, context)?;
            keys.set(js_string!("root"), statics.root.clone(), false, context)?;
            keys.set(js_string!("ctx"), statics.ctx.clone(), false, context)?;
            keys.set(js_string!("parent"), statics.parent.clone(), false, context)?;
            Ok((factory, ops, keys))
        })?;
        cx::with(|context| {
            let factory = factory
                .as_callable()
                .expect("cordis.js evaluates to a function");
            factory.call(&JsValue::undefined(), &[ops.into(), keys.into()], context)
        })
    })?;
    context.insert_data(Exports(exports.clone()));
    Ok(exports)
}

/// Engine continuations run as microtasks: a drain is queued whenever the ready queue fills.
fn install_scheduler() {
    cordis::task::reset_drain();
    cordis::task::set_scheduler(Some(Rc::new(|| {
        cx::try_with(|context| {
            context.enqueue_job(Job::PromiseJob(PromiseJob::new(|context| {
                cx::enter(context, || {
                    cordis::task::run_ready();
                });
                Ok(JsValue::undefined())
            })));
        })
        .is_some()
    })));
}
