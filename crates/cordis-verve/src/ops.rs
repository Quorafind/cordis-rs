//! The natives `cordis.js` calls: every one a thin translation between JavaScript values and
//! the engine. Each runs inside [`cx::enter`] (see [`crate::host::make_fn`]), so the engine can
//! reach Boa from wherever it is.

use std::rc::Rc;

use boa_engine::object::JsObject;
use boa_engine::object::builtins::JsArray;
use boa_engine::{Context, JsError, JsNativeError, JsResult, JsString, JsValue, js_string};
use cordis::host::{EffectValue, Host};
use cordis::{Callback, Effect, Fiber, Lookup, OnOptions, PluginSpec, Prop, Root, Runtime};

use crate::cx;
use crate::host::{
    EffectHandle, FiberHandle, JsHost, JsKey, RootHandle, RuntimeHandle, Statics, handle_object,
    make_fn,
};

type Op = fn(&JsValue, &[JsValue]) -> JsResult<JsValue>;

fn arg(args: &[JsValue], at: usize) -> JsValue {
    args.get(at).cloned().unwrap_or_default()
}

fn type_error(message: &str) -> JsError {
    JsNativeError::typ().with_message(message.to_owned()).into()
}

fn statics() -> Rc<Statics> {
    cx::with(|context| {
        context
            .get_data::<Rc<Statics>>()
            .cloned()
            .expect("cordis-verve is installed")
    })
}

fn root_of(value: &JsValue) -> JsResult<Root<JsHost>> {
    value
        .as_object()
        .and_then(|object| object.downcast_ref::<RootHandle>().map(|h| h.0.clone()))
        .ok_or_else(|| type_error("not a cordis root"))
}

fn fiber_of(value: &JsValue) -> JsResult<Fiber<JsHost>> {
    let statics = statics();
    cx::with(|context| {
        handle_object(value, &statics, context)
            .and_then(|object| object.downcast_ref::<FiberHandle>().map(|h| h.0.clone()))
    })
    .ok_or_else(|| type_error("not a cordis fiber"))
}

fn effect_of(value: &JsValue) -> JsResult<Effect<JsHost>> {
    let statics = statics();
    cx::with(|context| {
        handle_object(value, &statics, context)
            .and_then(|object| object.downcast_ref::<EffectHandle>().map(|h| h.0.clone()))
    })
    .ok_or_else(|| type_error("not a cordis effect"))
}

fn runtime_of(value: &JsValue) -> JsResult<Runtime<JsHost>> {
    let statics = statics();
    cx::with(|context| {
        handle_object(value, &statics, context)
            .and_then(|object| object.downcast_ref::<RuntimeHandle>().map(|h| h.0.clone()))
    })
    .ok_or_else(|| type_error("not a cordis runtime"))
}

fn object_of(value: &JsValue) -> JsResult<JsObject> {
    value.as_object().ok_or_else(|| type_error("not an object"))
}

fn text(value: &JsValue) -> JsResult<String> {
    cx::with(|context| Ok(value.to_string(context)?.to_std_string_escaped()))
}

fn array(items: Vec<JsValue>) -> JsValue {
    cx::with(|context| JsArray::from_iter(items, context).into())
}

fn this_of(value: JsValue) -> Option<JsValue> {
    (!value.is_null_or_undefined()).then_some(value)
}

fn args_of(value: &JsValue) -> JsResult<Vec<JsValue>> {
    let Some(object) = value.as_object() else {
        return Ok(Vec::new());
    };
    cx::with(|context| {
        let length = object
            .get(js_string!("length"), context)?
            .to_length(context)?;
        (0..length).map(|i| object.get(i, context)).collect()
    })
}

fn name_of(root: &Root<JsHost>, value: &JsValue) -> JsResult<String> {
    cx::with(|context| root.host().name_key(value, context))
}

fn impl_face(root: &Root<JsHost>, imp: &cordis::Impl<JsHost>) -> JsValue {
    let host = root.host();
    let fiber = host.fiber_value(&imp.fiber);
    let value = imp.value.borrow().clone();
    let check = imp.check.clone().unwrap_or_default();
    cx::with(|context| {
        let object = JsObject::with_object_proto(context.intrinsics());
        let _ = object.set(
            js_string!("name"),
            JsString::from(imp.name.as_str()),
            false,
            context,
        );
        let _ = object.set(js_string!("value"), value, false, context);
        let _ = object.set(js_string!("fiber"), fiber, false, context);
        let _ = object.set(js_string!("check"), check, false, context);
        object.into()
    })
}

fn effect_meta(effect: &Effect<JsHost>) -> JsValue {
    let children: Vec<JsValue> = effect.children().iter().map(effect_meta).collect();
    let children = array(children);
    cx::with(|context| {
        let object = JsObject::with_object_proto(context.intrinsics());
        let _ = object.set(
            js_string!("label"),
            JsString::from(effect.label()),
            false,
            context,
        );
        let _ = object.set(js_string!("children"), children, false, context);
        object.into()
    })
}

fn root_of_fiber(fiber: &Fiber<JsHost>) -> JsResult<Root<JsHost>> {
    fiber
        .root()
        .ok_or_else(|| type_error("the fiber's root is gone"))
}

// ─── Roots ───────────────────────────────────────────────────────────────────────────────

fn create_root(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let ctx = object_of(&arg(args, 0))?;
    let helpers = object_of(&arg(args, 1))?;
    let statics = statics();
    let host = JsHost {
        statics,
        helpers,
        root_ctx: Default::default(),
        names: Default::default(),
    };
    *host.root_ctx.borrow_mut() = Some(ctx.clone());
    let root = Root::new(host);
    root.set_ctx(ctx);
    Ok(JsObject::from_proto_and_data(None, RootHandle::new(root)).into())
}

fn root_fiber(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let fiber = root.fiber();
    let had_face = fiber.face().is_some();
    let face = root.host().fiber_value(&fiber);
    if !had_face {
        if let Some(ctx) = root.ctx() {
            let helpers = root.host().helpers.clone();
            cx::with(|context| -> JsResult<()> {
                let f = helpers.get(js_string!("rootFiberCtx"), context)?;
                if let Some(f) = f.as_callable() {
                    f.call(&JsValue::undefined(), &[face.clone(), ctx.into()], context)?;
                }
                Ok(())
            })?;
        }
    }
    Ok(face)
}

fn forget_root_disposables(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    root_of(&arg(args, 0))?.fiber().forget_disposables();
    Ok(JsValue::undefined())
}

// ─── Fibers ──────────────────────────────────────────────────────────────────────────────

fn fiber_uid(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(match fiber_of(&arg(args, 0))?.uid() {
        Some(uid) => JsValue::from(uid),
        None => JsValue::null(),
    })
}

fn fiber_state(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(JsValue::from(fiber_of(&arg(args, 0))?.state().as_number()))
}

fn fiber_runtime(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let Some(runtime) = fiber.runtime() else {
        return Ok(JsValue::null());
    };
    let root = root_of_fiber(&fiber)?;
    Ok(root.host().runtime_value(&runtime))
}

fn fiber_inject(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let inject: Vec<(String, Option<JsValue>)> = fiber.inject().to_vec();
    cx::with(|context| {
        let object = JsObject::with_null_proto();
        for (name, config) in inject {
            object.set(
                JsString::from(name),
                config.unwrap_or(JsValue::null()),
                false,
                context,
            )?;
        }
        Ok(object.into())
    })
}

fn fiber_config(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(fiber_of(&arg(args, 0))?.config().unwrap_or_default())
}

fn fiber_set_config(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    fiber_of(&arg(args, 0))?.set_config(arg(args, 1));
    Ok(JsValue::undefined())
}

fn store_object(
    root: &Root<JsHost>,
    store: Option<Vec<(String, Rc<cordis::Impl<JsHost>>)>>,
) -> JsResult<JsValue> {
    let Some(store) = store else {
        return Ok(JsValue::undefined());
    };
    let faces: Vec<(String, JsValue)> = store
        .iter()
        .map(|(name, imp)| (name.clone(), impl_face(root, imp)))
        .collect();
    cx::with(|context| {
        let object = JsObject::with_null_proto();
        for (name, face) in faces {
            object.set(JsString::from(name), face, false, context)?;
        }
        Ok(object.into())
    })
}

fn fiber_store(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    store_object(&root, fiber.store())
}

fn fiber_checked(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    store_object(&root, Some(fiber.checked()))
}

fn fiber_inertia(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let Some(inertia) = fiber.inertia() else {
        return Ok(JsValue::undefined());
    };
    let root = root_of_fiber(&fiber)?;
    let mapped = cordis::task::spawn(async move {
        inertia.await;
        Ok(JsValue::undefined())
    });
    Ok(root.host().promise(mapped))
}

fn fiber_error(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    Ok(match fiber.error() {
        Some(error) => cx::with(|context| error.into_opaque(context))?,
        None => JsValue::undefined(),
    })
}

fn fiber_set_error(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let value = arg(args, 1);
    fiber.set_error((!value.is_undefined()).then(|| JsError::from_opaque(value)));
    Ok(JsValue::undefined())
}

fn fiber_dispose(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let Some(effect) = fiber.dispose_effect() else {
        return Ok(JsValue::undefined());
    };
    let root = root_of_fiber(&fiber)?;
    Ok(root.host().effect_value(&effect))
}

fn fiber_name(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(JsString::from(fiber_of(&arg(args, 0))?.name()).into())
}

fn fiber_update_hooks(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    let hooks: Vec<JsValue> = fiber
        .update_hooks()
        .values()
        .iter()
        .map(|callback| callback.to_value(root.host(), ""))
        .collect();
    Ok(array(hooks))
}

fn fiber_disposables(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    let host = root.host();
    let values: Vec<JsValue> = fiber
        .disposables()
        .into_iter()
        .map(|disposer| match disposer {
            cordis::Disposer::Effect(effect) => host.effect_value(&effect),
            cordis::Disposer::Value(value) => value,
            cordis::Disposer::Native(f) => host.native_function(
                "",
                Rc::new(move |_, _| {
                    f()?;
                    Ok(JsValue::undefined())
                }),
            ),
        })
        .collect();
    Ok(array(values))
}

fn fiber_assert_active(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    fiber_of(&arg(args, 0))?.assert_active()?;
    Ok(JsValue::undefined())
}

fn fiber_effect(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let execute = arg(args, 1);
    let label = text(&arg(args, 2))?;
    let root = root_of_fiber(&fiber)?;
    let weak = fiber.downgrade();
    let body: Rc<dyn Fn() -> Result<EffectValue<JsHost>, JsError>> = Rc::new(move || {
        let Some(fiber) = weak.upgrade() else {
            return Ok(EffectValue::Nullish);
        };
        let root = root_of_fiber(&fiber)?;
        let host = root.host();
        let this = host.fiber_value(&fiber);
        let value = host.call(&execute, &this, &[])?;
        Ok(host.classify(value))
    });
    let effect = fiber.effect(body, label)?;
    Ok(root.host().effect_value(&effect))
}

fn fiber_effects(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let metas: Vec<JsValue> = fiber.effects().iter().map(effect_meta).collect();
    Ok(array(metas))
}

fn fiber_await(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    let waited = fiber.wait();
    let task = cordis::task::spawn(async move { waited.await.map(|()| JsValue::undefined()) });
    Ok(root.host().promise(task))
}

fn fiber_restart(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let root = root_of_fiber(&fiber)?;
    Ok(root.task_value(fiber.restart()))
}

fn fiber_update(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let fiber = fiber_of(&arg(args, 0))?;
    let no_save = arg(args, 2).to_boolean();
    fiber.update(arg(args, 1), no_save)?;
    Ok(JsValue::undefined())
}

// ─── Effects ─────────────────────────────────────────────────────────────────────────────

fn effect_dispose(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let effect = effect_of(&arg(args, 0))?;
    let pending = effect.dispose()?;
    Ok(match pending {
        Some(task) => {
            let mapped =
                cordis::task::spawn(async move { task.await.map(|()| JsValue::undefined()) });
            crate::host::promise_of(mapped)
        }
        None => JsValue::undefined(),
    })
}

fn effect_dispose_now(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let effect = effect_of(&arg(args, 0))?;
    let pending = effect.dispose_now()?;
    Ok(match pending {
        Some(task) => {
            let mapped =
                cordis::task::spawn(async move { task.await.map(|()| JsValue::undefined()) });
            crate::host::promise_of(mapped)
        }
        None => JsValue::undefined(),
    })
}

fn effect_task(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let effect = effect_of(&arg(args, 0))?;
    Ok(match effect.task() {
        Some(task) => {
            let mapped =
                cordis::task::spawn(async move { task.await.map(|()| JsValue::undefined()) });
            crate::host::promise_of(mapped)
        }
        None => JsValue::undefined(),
    })
}

fn effect_meta_op(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(effect_meta(&effect_of(&arg(args, 0))?))
}

// ─── Runtimes ────────────────────────────────────────────────────────────────────────────

fn runtime_name(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let runtime = runtime_of(&arg(args, 0))?;
    Ok(match runtime.name() {
        Some(name) => JsString::from(name).into(),
        None => JsValue::undefined(),
    })
}

fn runtime_fibers(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let runtime = runtime_of(&arg(args, 0))?;
    let fibers = runtime.fibers();
    let Some(root) = fibers.first().and_then(Fiber::root) else {
        return Ok(array(Vec::new()));
    };
    let faces: Vec<JsValue> = fibers.iter().map(|f| root.host().fiber_value(f)).collect();
    Ok(array(faces))
}

// ─── Events ──────────────────────────────────────────────────────────────────────────────

fn hooks(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let host = root.host();
    let table: Vec<(JsValue, Vec<JsValue>)> = root
        .hooks()
        .into_iter()
        .map(|(name, list)| {
            let name_value = host.event_name(&name);
            let entries = list
                .iter()
                .map(|hook| {
                    let callback = hook.callback.to_value(host, "");
                    let ctx: JsValue = hook.ctx.clone().into();
                    cx::with(|context| {
                        let object = JsObject::with_object_proto(context.intrinsics());
                        let _ = object.set(js_string!("ctx"), ctx, false, context);
                        let _ = object.set(js_string!("callback"), callback, false, context);
                        let _ = object.set(js_string!("global"), hook.global, false, context);
                        let _ = object.set(js_string!("prepend"), hook.prepend, false, context);
                        JsValue::from(object)
                    })
                })
                .collect();
            (name_value, entries)
        })
        .collect();
    cx::with(|context| {
        let object = JsObject::with_null_proto();
        for (name, entries) in table {
            let list = JsArray::from_iter(entries, context);
            let key = name.to_property_key(context)?;
            object.set(key, list, false, context)?;
        }
        Ok(object.into())
    })
}

fn dispatch(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let kind = text(&arg(args, 1))?;
    let this = this_of(arg(args, 2));
    let name = name_of(&root, &arg(args, 3))?;
    let rest = args_of(&arg(args, 4))?;
    let callbacks = root.resolve(
        &kind,
        this.as_ref(),
        &name,
        &rest,
        &cordis::Filter::FromThis,
    )?;
    let values: Vec<JsValue> = callbacks
        .iter()
        .map(|callback| callback.to_value(root.host(), ""))
        .collect();
    Ok(array(values))
}

fn emit(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let this = this_of(arg(args, 1));
    let name = name_of(&root, &arg(args, 2))?;
    let rest = args_of(&arg(args, 3))?;
    root.emit(this, &name, &rest)?;
    Ok(JsValue::undefined())
}

fn bail(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let this = this_of(arg(args, 1));
    let name = name_of(&root, &arg(args, 2))?;
    let rest = args_of(&arg(args, 3))?;
    root.bail(this, &name, &rest)
}

fn parallel(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let this = this_of(arg(args, 1));
    let name = name_of(&root, &arg(args, 2))?;
    let rest = args_of(&arg(args, 3))?;
    Ok(root.task_value(root.parallel(this, &name, rest)))
}

fn serial(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let this = this_of(arg(args, 1));
    let name = name_of(&root, &arg(args, 2))?;
    let rest = args_of(&arg(args, 3))?;
    let task = root.serial(this, &name, rest);
    Ok(root.host().promise(task))
}

fn waterfall(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let this = this_of(arg(args, 1));
    let name = name_of(&root, &arg(args, 2))?;
    let rest = args_of(&arg(args, 3))?;
    let inner = arg(args, 4);
    root.waterfall(this, &name, rest, Callback::Value(inner))
}

fn on(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = name_of(&root, &arg(args, 2))?;
    let listener = arg(args, 3);
    let options = arg(args, 4);
    let label = text(&arg(args, 5))?;
    let (prepend, global) = cx::with(|context| -> JsResult<(bool, bool)> {
        let Some(object) = options.as_object() else {
            return Ok((false, false));
        };
        Ok((
            object.get(js_string!("prepend"), context)?.to_boolean(),
            object.get(js_string!("global"), context)?.to_boolean(),
        ))
    })?;
    let disposer = root.on(
        &ctx,
        &name,
        Callback::Value(listener),
        OnOptions {
            prepend,
            global,
            value: Some(options),
            label: Some(label),
        },
    )?;
    let host = root.host();
    Ok(match disposer {
        cordis::Disposer::Effect(effect) => host.effect_value(&effect),
        cordis::Disposer::Value(value) => value,
        cordis::Disposer::Native(f) => host.native_function(
            "",
            Rc::new(move |_, _| {
                f()?;
                Ok(JsValue::undefined())
            }),
        ),
    })
}

// ─── Reflect ─────────────────────────────────────────────────────────────────────────────

fn has_prop(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let name = arg(args, 1);
    if !name.is_string() {
        return Ok(false.into());
    }
    Ok(root.prop(&text(&name)?).is_some().into())
}

fn prop(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let name = arg(args, 1);
    if !name.is_string() {
        return Ok(JsValue::undefined());
    }
    let Some(prop) = root.prop(&text(&name)?) else {
        return Ok(JsValue::undefined());
    };
    let host = root.host();
    let (get, set) = match &prop {
        Prop::Service => (None, None),
        Prop::Accessor { get, set } => (
            get.as_ref().map(|g| g.to_value(host, "get")),
            set.as_ref().map(|s| s.to_value(host, "set")),
        ),
    };
    cx::with(|context| {
        let object = JsObject::with_object_proto(context.intrinsics());
        object.set(
            js_string!("type"),
            JsString::from(prop.kind()),
            false,
            context,
        )?;
        if let Some(get) = get {
            object.set(js_string!("get"), get, false, context)?;
        }
        if let Some(set) = set {
            object.set(js_string!("set"), set, false, context)?;
        }
        Ok(object.into())
    })
}

fn prop_names(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let names: Vec<JsValue> = root
        .props()
        .into_iter()
        .map(|(name, _)| JsString::from(name).into())
        .collect();
    Ok(array(names))
}

fn lookup(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let fiber = fiber_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    let found = root.lookup(&fiber, &name);
    cx::with(|context| {
        Ok(match found {
            Lookup::Found(value) => JsArray::from_iter([value], context).into(),
            Lookup::Inactive => {
                let list = JsArray::new(context)?;
                list.set(js_string!("inactive"), true, false, context)?;
                list.into()
            }
            Lookup::Missing => JsArray::new(context)?.into(),
        })
    })
}

fn get_service(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    let strict = arg(args, 3).to_boolean();
    Ok(root.get_service(&ctx, &name, strict).unwrap_or_default())
}

fn get_impl(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    let strict = arg(args, 3).to_boolean();
    Ok(match root.get_impl(&ctx, &name, strict) {
        Some(imp) => impl_face(&root, &imp),
        None => JsValue::undefined(),
    })
}

fn set_service(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    root.set_service(&ctx, &name, arg(args, 3))?;
    Ok(true.into())
}

fn provide(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    let check = arg(args, 4);
    let check = (!check.is_null_or_undefined()).then_some(check);
    let effect = root.provide(&ctx, &name, arg(args, 3), check)?;
    Ok(root.host().effect_value(&effect))
}

fn notify(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let names: Vec<String> = args_of(&arg(args, 2))?
        .iter()
        .map(text)
        .collect::<JsResult<_>>()?;
    let fibers = root.notify(&ctx, &names)?;
    let faces: Vec<JsValue> = fibers.iter().map(|f| root.host().fiber_value(f)).collect();
    Ok(array(faces))
}

fn accessor(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let name = text(&arg(args, 2))?;
    let get = arg(args, 3);
    let set = arg(args, 4);
    let get = (!get.is_null_or_undefined()).then_some(Callback::Value(get));
    let set = (!set.is_null_or_undefined()).then_some(Callback::Value(set));
    let effect = root.accessor(&ctx, &name, get, set)?;
    Ok(root.host().effect_value(&effect))
}

fn store(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let faces: Vec<(JsKey, JsValue)> = root
        .impls()
        .iter()
        .map(|(key, imp)| (key.clone(), impl_face(&root, imp)))
        .collect();
    cx::with(|context| {
        let object = JsObject::with_null_proto();
        for (key, face) in faces {
            match key {
                JsKey::Symbol(symbol) => object.set(symbol, face, false, context)?,
                JsKey::Text(text) => object.set(text, face, false, context)?,
            };
        }
        Ok(object.into())
    })
}

// ─── Registry ────────────────────────────────────────────────────────────────────────────

fn counter(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    Ok(JsValue::from(root.next_counter()))
}

fn runtime_count(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    Ok(JsValue::from(root_of(&arg(args, 0))?.runtime_count() as u32))
}

fn runtime_get(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let Some(key) = arg(args, 1).as_object() else {
        return Ok(JsValue::undefined());
    };
    Ok(match root.runtime(&key) {
        Some(runtime) => root.host().runtime_value(&runtime),
        None => JsValue::undefined(),
    })
}

fn runtime_delete(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let Some(key) = arg(args, 1).as_object() else {
        return Ok(JsValue::undefined());
    };
    let runtime = root.runtime(&key);
    let face = runtime.map(|runtime| root.host().runtime_value(&runtime));
    root.delete_runtime(&key);
    Ok(face.unwrap_or_default())
}

fn runtimes(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let pairs: Vec<(JsValue, JsValue)> = root
        .runtimes()
        .iter()
        .map(|runtime| {
            (
                runtime.callback().clone(),
                root.host().runtime_value(runtime),
            )
        })
        .collect();
    cx::with(|context| {
        let entries: Vec<JsValue> = pairs
            .into_iter()
            .map(|(key, face)| JsArray::from_iter([key, face], context).into())
            .collect();
        Ok(JsArray::from_iter(entries, context).into())
    })
}

fn plugin(_: &JsValue, args: &[JsValue]) -> JsResult<JsValue> {
    let root = root_of(&arg(args, 0))?;
    let ctx = object_of(&arg(args, 1))?;
    let callback = arg(args, 2);
    let key = object_of(&callback)?;
    let name = arg(args, 3);
    let name = if name.is_string() {
        Some(text(&name)?)
    } else {
        None
    };
    let schema = arg(args, 4);
    let schema = (!schema.is_null_or_undefined()).then_some(schema);
    let mut inject = Vec::new();
    for entry in args_of(&arg(args, 5))? {
        let pair = args_of(&entry)?;
        let name = text(&pair.first().cloned().unwrap_or_default())?;
        let config = pair.get(1).cloned().unwrap_or_default();
        inject.push((name, (!config.is_null_or_undefined()).then_some(config)));
    }
    let config = arg(args, 6);
    let fiber = root.plugin(
        &ctx,
        PluginSpec {
            key,
            callback,
            name,
            schema,
            inject,
        },
        config,
    )?;
    Ok(root.host().fiber_value(&fiber))
}

/// Every op, by the name `cordis.js` calls it.
const OPS: &[(&str, usize, Op)] = &[
    ("createRoot", 2, create_root),
    ("rootFiber", 1, root_fiber),
    ("forgetRootDisposables", 1, forget_root_disposables),
    ("fiberUid", 1, fiber_uid),
    ("fiberState", 1, fiber_state),
    ("fiberRuntime", 1, fiber_runtime),
    ("fiberInject", 1, fiber_inject),
    ("fiberConfig", 1, fiber_config),
    ("fiberSetConfig", 2, fiber_set_config),
    ("fiberStore", 1, fiber_store),
    ("fiberChecked", 1, fiber_checked),
    ("fiberInertia", 1, fiber_inertia),
    ("fiberError", 1, fiber_error),
    ("fiberSetError", 2, fiber_set_error),
    ("fiberDispose", 1, fiber_dispose),
    ("fiberName", 1, fiber_name),
    ("fiberUpdateHooks", 1, fiber_update_hooks),
    ("fiberDisposables", 1, fiber_disposables),
    ("fiberAssertActive", 1, fiber_assert_active),
    ("fiberEffect", 3, fiber_effect),
    ("fiberEffects", 1, fiber_effects),
    ("fiberAwait", 1, fiber_await),
    ("fiberRestart", 1, fiber_restart),
    ("fiberUpdate", 3, fiber_update),
    ("effectDispose", 1, effect_dispose),
    ("effectDisposeNow", 1, effect_dispose_now),
    ("effectTask", 1, effect_task),
    ("effectMeta", 1, effect_meta_op),
    ("runtimeName", 1, runtime_name),
    ("runtimeFibers", 1, runtime_fibers),
    ("hooks", 1, hooks),
    ("dispatch", 5, dispatch),
    ("emit", 4, emit),
    ("bail", 4, bail),
    ("parallel", 4, parallel),
    ("serial", 4, serial),
    ("waterfall", 5, waterfall),
    ("on", 6, on),
    ("hasProp", 2, has_prop),
    ("prop", 2, prop),
    ("propNames", 1, prop_names),
    ("lookup", 3, lookup),
    ("getService", 4, get_service),
    ("getImpl", 4, get_impl),
    ("setService", 4, set_service),
    ("provide", 5, provide),
    ("notify", 3, notify),
    ("accessor", 5, accessor),
    ("store", 1, store),
    ("counter", 1, counter),
    ("runtimeCount", 1, runtime_count),
    ("runtimeGet", 2, runtime_get),
    ("runtimeDelete", 2, runtime_delete),
    ("runtimes", 1, runtimes),
    ("plugin", 7, plugin),
];

/// The ops object handed to `cordis.js`.
pub(crate) fn ops(context: &mut Context) -> JsResult<JsObject> {
    let object = JsObject::with_null_proto();
    for (name, length, op) in OPS {
        let op = *op;
        let function = make_fn(context, name, *length, move |this, args| op(this, args));
        object.set(JsString::from(*name), function, false, context)?;
    }
    Ok(object)
}
