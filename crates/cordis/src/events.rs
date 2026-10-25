//! Events: `ctx.on`, and `emit`, `parallel`, `serial`, `bail` and `waterfall`.
//!
//! Listeners are kept per event name in registration order (`prepend` puts one first).
//! Dispatch resolves the list once, up front: a `this` carrying a `[Context.filter]` keeps only
//! the listeners whose context it accepts (global ones always run), and an event outside
//! `internal/` is announced to `internal/dispatch` listeners first. `bail` and `serial` stop at
//! the first answer that is not `null`, `false` or `undefined`.

use std::cell::RefCell;
use std::collections::VecDeque;
use std::rc::{Rc, Weak};

use crate::effect::Disposer;
use crate::host::{EffectValue, Host};
use crate::root::{Callback, Hook, Root, RootInner};
use crate::task::{self, Task};

/// Which listeners a dispatch keeps.
pub enum Filter<H: Host> {
    /// Whatever `this[Context.filter]` says (none when there is no `this`).
    FromThis,
    /// The engine's own test: `notify` keeps listeners whose context shares the service's
    /// isolation key.
    Native(Rc<dyn Fn(&H::Ctx) -> bool>),
}

/// How `ctx.on` was asked to register.
pub struct OnOptions<H: Host> {
    pub prepend: bool,
    pub global: bool,
    /// The options value itself, for `internal/listener` listeners and `_hooks`.
    pub value: Option<H::Value>,
    /// The effect's label; `ctx.on("name")` when absent.
    pub label: Option<String>,
}

impl<H: Host> Default for OnOptions<H> {
    fn default() -> Self {
        Self {
            prepend: false,
            global: false,
            value: None,
            label: None,
        }
    }
}

/// A registration `ctx.on` is making, for the built-in `internal/listener` listener to see.
pub(crate) struct Listening<H: Host> {
    name: String,
    ctx: H::Ctx,
    callback: Callback<H>,
    global: bool,
    prepend: bool,
}

thread_local! {
    /// Registrations in progress, innermost last. Keyed by nothing: the built-in listener
    /// only ever answers the registration that is asking.
    static LISTENING: RefCell<Vec<Rc<dyn std::any::Any>>> = const { RefCell::new(Vec::new()) };
}

fn json_string(text: &str) -> String {
    let mut out = String::with_capacity(text.len() + 2);
    out.push('"');
    for ch in text.chars() {
        match ch {
            '"' => out.push_str("\\\""),
            '\\' => out.push_str("\\\\"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            '\t' => out.push_str("\\t"),
            '\u{8}' => out.push_str("\\b"),
            '\u{c}' => out.push_str("\\f"),
            c if (c as u32) < 0x20 => out.push_str(&format!("\\u{:04x}", c as u32)),
            c => out.push(c),
        }
    }
    out.push('"');
    out
}

/// `JSON.stringify(name)`, as effect labels quote names.
pub fn quote(name: &str) -> String {
    json_string(name)
}

impl<H: Host> Root<H> {
    /// The two listeners `new EventsService()` registers on the root, and never undoes:
    /// `internal/listener` keeps a fiber's own `internal/update` listeners on the fiber, and a
    /// global, prepended `internal/update` runs them before the update itself.
    pub(crate) fn install_builtin_hooks(&self, ctx: &H::Ctx) {
        let root = self.downgrade();
        let listener: Callback<H> = Callback::Native(Rc::new(move |_, _| {
            let root = root.upgrade().map(Root::from_inner);
            let Some(root) = root else {
                panic!("a hook outlived its root");
            };
            let host = root.host();
            let listening = LISTENING.with(|stack| stack.borrow().last().cloned());
            let listening = listening.and_then(|l| l.downcast::<Listening<H>>().ok());
            let Some(listening) = listening else {
                return Ok(host.undefined());
            };
            if listening.name != "internal/update" || listening.global {
                return Ok(host.undefined());
            }
            let Some(fiber) = host.fiber_of(&listening.ctx) else {
                return Ok(host.undefined());
            };
            let hooks = fiber.downgrade();
            let sn = if listening.prepend {
                fiber.update_hooks().unshift(listening.callback.clone())
            } else {
                fiber.update_hooks().push(listening.callback.clone())
            };
            let remove: crate::host::NativeFn<H> = {
                let root = root.downgrade();
                Rc::new(move |_, _| {
                    let removed = hooks
                        .upgrade()
                        .is_some_and(|fiber| fiber.update_hooks().remove(sn));
                    let root = root.upgrade().map(Root::from_inner);
                    Ok(root
                        .map(|root| root.host().boolean(removed))
                        .expect("a hook outlived its root"))
                })
            };
            Ok(host.native_function("", remove))
        }));
        let root = self.downgrade();
        let update: Callback<H> = Callback::Native(Rc::new(move |this, args| {
            let root = root
                .upgrade()
                .map(Root::from_inner)
                .expect("a hook outlived its root");
            let host = root.host();
            let next = args.get(2).cloned().unwrap_or_else(|| host.undefined());
            let queue: VecDeque<Callback<H>> = host
                .fiber_from_value(this)
                .map(|fiber| fiber.update_hooks().values())
                .unwrap_or_default()
                .into();
            let args = vec![
                args.first().cloned().unwrap_or_else(|| host.undefined()),
                args.get(1).cloned().unwrap_or_else(|| host.undefined()),
            ];
            Chain::start(
                &root,
                queue,
                this.clone(),
                args,
                ChainEnd::Next(next),
                "_next",
            )
        }));
        let mut hooks = self.inner.hooks.borrow_mut();
        hooks
            .entry("internal/listener".into())
            .or_default()
            .push(Rc::new(Hook {
                ctx: ctx.clone(),
                callback: listener,
                global: false,
                prepend: false,
                options: None,
            }));
        hooks.entry("internal/update".into()).or_default().insert(
            0,
            Rc::new(Hook {
                ctx: ctx.clone(),
                callback: update,
                global: true,
                prepend: true,
                options: None,
            }),
        );
    }

    /// `_resolve`: the callbacks `name` runs for `this`, decided before any runs.
    pub fn resolve(
        &self,
        kind: &str,
        this: Option<&H::Value>,
        name: &str,
        args: &[H::Value],
        filter: &Filter<H>,
    ) -> Result<Vec<Callback<H>>, H::Error> {
        let host = self.host();
        if !name.starts_with("internal/") && !self.hooks_of("internal/dispatch").is_empty() {
            let this_value = this.cloned().unwrap_or_else(|| host.null());
            self.emit(
                None,
                "internal/dispatch",
                &[
                    host.string(kind),
                    host.event_name(name),
                    host.array(args.to_vec()),
                    this_value,
                ],
            )?;
        }
        let from_this = match filter {
            Filter::FromThis => this.and_then(|this| host.event_filter(this)),
            Filter::Native(_) => None,
        };
        let this_value = this.cloned().unwrap_or_else(|| host.null());
        let mut callbacks = Vec::new();
        for hook in self.hooks_of(name) {
            let keep = hook.global
                || match filter {
                    Filter::Native(test) => test(&hook.ctx),
                    Filter::FromThis => match &from_this {
                        None => true,
                        Some(test) => {
                            let answer =
                                host.call(test, &this_value, &[host.ctx_value(&hook.ctx)])?;
                            host.is_truthy(&answer)
                        }
                    },
                };
            if keep {
                callbacks.push(hook.callback.clone());
            }
        }
        Ok(callbacks)
    }

    /// `emit`: every listener, in order; a throw stops the rest.
    pub fn emit(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: &[H::Value],
    ) -> Result<(), H::Error> {
        self.emit_filtered(this, name, args, &Filter::FromThis)
    }

    pub fn emit_filtered(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: &[H::Value],
        filter: &Filter<H>,
    ) -> Result<(), H::Error> {
        let host = self.host();
        let callbacks = self.resolve("emit", this.as_ref(), name, args, filter)?;
        let this = this.unwrap_or_else(|| host.null());
        for callback in callbacks {
            callback.call(host, &this, args)?;
        }
        Ok(())
    }

    /// `bail`: the first answer that is not `null`, `false` or `undefined`.
    pub fn bail(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: &[H::Value],
    ) -> Result<H::Value, H::Error> {
        let host = self.host();
        let callbacks = self.resolve("bail", this.as_ref(), name, args, &Filter::FromThis)?;
        let this = this.unwrap_or_else(|| host.null());
        for callback in callbacks {
            let result = callback.call(host, &this, args)?;
            if host.is_bailed(&result) {
                return Ok(result);
            }
        }
        Ok(host.undefined())
    }

    /// `parallel`: every listener started at once; rejections gathered into an
    /// `AggregateError`.
    pub fn parallel(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: Vec<H::Value>,
    ) -> Task<Result<(), H::Error>> {
        let host = self.host();
        let scope = host.scope();
        let resolved = self.resolve("emit", this.as_ref(), name, &args, &Filter::FromThis);
        let this = this.unwrap_or_else(|| host.null());
        let started: Result<Vec<Task<Result<(), H::Error>>>, H::Error> =
            resolved.map(|callbacks| {
                callbacks
                    .into_iter()
                    .map(|callback| {
                        let outcome = callback.call(host, &this, &args);
                        match outcome {
                            Ok(value) if host.is_thenable(&value) => {
                                let settled = host.await_value(value);
                                task::spawn_in(
                                    scope.clone(),
                                    async move { settled.await.map(|_| ()) },
                                )
                            }
                            Ok(_) => task::spawn(async { Ok(()) }),
                            Err(error) => task::spawn(async move { Err(error) }),
                        }
                    })
                    .collect()
            });
        let root = self.downgrade();
        task::spawn_in(scope, async move {
            let started = started?;
            let mut errors = Vec::new();
            for task in started {
                if let Err(error) = task.await {
                    errors.push(error);
                }
            }
            if errors.is_empty() {
                return Ok(());
            }
            let root = root
                .upgrade()
                .map(Root::from_inner)
                .expect("a dispatch outlived its root");
            Err(root.host().aggregate_error(errors))
        })
    }

    /// `serial`: one listener at a time, each awaited; the first answer that is not `null`,
    /// `false` or `undefined` ends it.
    pub fn serial(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: Vec<H::Value>,
    ) -> Task<Result<H::Value, H::Error>> {
        let host = self.host();
        let resolved = self.resolve("serial", this.as_ref(), name, &args, &Filter::FromThis);
        let this = this.unwrap_or_else(|| host.null());
        let root = self.clone();
        task::spawn_in(host.scope(), async move {
            let host = root.host();
            for callback in resolved? {
                let mut result = callback.call(host, &this, &args)?;
                result = host.await_value(result).await?;
                if host.is_bailed(&result) {
                    return Ok(result);
                }
            }
            Ok(host.undefined())
        })
    }

    /// `waterfall`: each listener gets the arguments and a `next` that runs the rest; after
    /// the last, `inner` runs.
    pub fn waterfall(
        &self,
        this: Option<H::Value>,
        name: &str,
        args: Vec<H::Value>,
        inner: Callback<H>,
    ) -> Result<H::Value, H::Error> {
        let host = self.host();
        let callbacks = self.resolve("waterfall", this.as_ref(), name, &args, &Filter::FromThis)?;
        let this = this.unwrap_or_else(|| host.null());
        Chain::start(
            self,
            VecDeque::from(callbacks),
            this,
            args,
            ChainEnd::Inner(inner),
            "next",
        )
    }

    /// `ctx.on(name, listener, options)`: offered to `internal/listener` first (which keeps
    /// a fiber's own `internal/update` listeners on the fiber), otherwise registered as an
    /// effect of the context's fiber.
    pub fn on(
        &self,
        ctx: &H::Ctx,
        name: &str,
        callback: Callback<H>,
        options: OnOptions<H>,
    ) -> Result<Disposer<H>, H::Error> {
        let host = self.host();
        let fiber = host.fiber_of(ctx).expect("a context has its fiber");
        fiber.assert_active()?;
        let listening: Rc<dyn std::any::Any> = Rc::new(Listening::<H> {
            name: name.to_owned(),
            ctx: ctx.clone(),
            callback: callback.clone(),
            global: options.global,
            prepend: options.prepend,
        });
        LISTENING.with(|stack| stack.borrow_mut().push(listening));
        let args = [
            host.event_name(name),
            callback.to_value(host, ""),
            options.value.clone().unwrap_or_else(|| host.undefined()),
        ];
        let bailed = self.bail(Some(host.ctx_value(ctx)), "internal/listener", &args);
        LISTENING.with(|stack| stack.borrow_mut().pop());
        let bailed = bailed?;
        if host.is_bailed(&bailed) {
            return Ok(host.as_disposer(&bailed).unwrap_or(Disposer::Value(bailed)));
        }
        let label = options
            .label
            .clone()
            .unwrap_or_else(|| format!("ctx.on({})", quote(name)));
        let hook = Rc::new(Hook {
            ctx: ctx.clone(),
            callback: callback.clone(),
            global: options.global,
            prepend: options.prepend,
            options: options.value.clone(),
        });
        let root = self.downgrade();
        let name = name.to_owned();
        let execute: Rc<dyn Fn() -> Result<EffectValue<H>, H::Error>> = Rc::new(move || {
            let root = root
                .upgrade()
                .map(Root::from_inner)
                .expect("a hook outlived its root");
            {
                let mut hooks = root.inner.hooks.borrow_mut();
                let list = hooks.entry(name.clone()).or_default();
                if hook.prepend {
                    list.insert(0, hook.clone());
                } else {
                    list.push(hook.clone());
                }
            }
            let weak = root.downgrade();
            let name = name.clone();
            let callback = hook.callback.clone();
            Ok(EffectValue::Dispose(Disposer::Native(Rc::new(move || {
                if let Some(root) = weak.upgrade().map(Root::from_inner) {
                    root.unregister(&name, &callback);
                }
                Ok(None)
            }))))
        });
        Ok(Disposer::Effect(fiber.effect(execute, label)?))
    }

    /// Removes the first listener of `name` that is `callback`.
    pub fn unregister(&self, name: &str, callback: &Callback<H>) -> bool {
        let host = self.host();
        let mut hooks = self.inner.hooks.borrow_mut();
        let Some(list) = hooks.get_mut(name) else {
            return false;
        };
        match list
            .iter()
            .position(|hook| hook.callback.same(callback, host))
        {
            Some(at) => {
                list.remove(at);
                true
            }
            None => false,
        }
    }
}

/// How a chain ends once its listeners are spent.
enum ChainEnd<H: Host> {
    /// `waterfall`'s `inner(...args)`.
    Inner(Callback<H>),
    /// The built-in `internal/update` listener's `next.call(this, ...args, _next)`.
    Next(H::Value),
}

/// A running `waterfall`: each listener is handed a `next` that runs the rest. The chain holds
/// itself only weakly, so it lives exactly as long as some `next` it handed out does.
struct Chain<H: Host> {
    me: Weak<Chain<H>>,
    root: Weak<RootInner<H>>,
    queue: RefCell<VecDeque<Callback<H>>>,
    this: H::Value,
    args: Vec<H::Value>,
    end: ChainEnd<H>,
    name: &'static str,
}

impl<H: Host> Chain<H> {
    fn start(
        root: &Root<H>,
        queue: VecDeque<Callback<H>>,
        this: H::Value,
        args: Vec<H::Value>,
        end: ChainEnd<H>,
        name: &'static str,
    ) -> Result<H::Value, H::Error> {
        let chain = Rc::new_cyclic(|me| Chain {
            me: me.clone(),
            root: root.downgrade(),
            queue: RefCell::new(queue),
            this,
            args,
            end,
            name,
        });
        chain.step()
    }

    fn step(&self) -> Result<H::Value, H::Error> {
        let root = self
            .root
            .upgrade()
            .map(Root::from_inner)
            .expect("a dispatch outlived its root");
        let host = root.host();
        let me = self.me.upgrade().expect("a running chain is held");
        let next = host.native_function(self.name, Rc::new(move |_, _| me.step()));
        let callback = self.queue.borrow_mut().pop_front();
        let mut full = self.args.clone();
        match callback {
            Some(callback) => {
                full.push(next);
                callback.call(host, &self.this, &full)
            }
            None => match &self.end {
                ChainEnd::Inner(inner) => inner.call(host, &host.undefined(), &self.args),
                ChainEnd::Next(f) => {
                    full.push(next);
                    host.call(f, &self.this, &full)
                }
            },
        }
    }
}
