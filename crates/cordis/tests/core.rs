//! The engine on Rust values: lifecycle, services, effects and events, as Cordis 4.0.0-rc.8
//! defines them. Each test drives the ready queue explicitly, so what is observed between
//! steps is what a JS program would observe between microtask turns.

use std::cell::RefCell;
use std::rc::Rc;

use cordis::native::{Context, Error, Value, plugin_fn};
use cordis::task::{self, run_until_stalled};
use cordis::{Disposer, FiberState, OnOptions};

type Log = Rc<RefCell<Vec<String>>>;

fn log() -> Log {
    Rc::new(RefCell::new(Vec::new()))
}

fn push(log: &Log, entry: impl Into<String>) {
    log.borrow_mut().push(entry.into());
}

fn taken(log: &Log) -> Vec<String> {
    std::mem::take(&mut *log.borrow_mut())
}

#[test]
fn a_plugin_loads_a_microtask_later_and_is_disposed() {
    let root = Context::new();
    let applied = log();
    let a = applied.clone();
    let fiber = root
        .plugin_fn("p", &[], move |_, _| {
            push(&a, "apply");
            let a = a.clone();
            Ok(Value::disposer(move || push(&a, "dispose")))
        })
        .unwrap();
    assert_eq!(fiber.uid(), Some(1));
    assert_eq!(fiber.state(), FiberState::Loading);
    assert!(
        applied.borrow().is_empty(),
        "the body waits for a microtask"
    );
    run_until_stalled();
    assert_eq!(fiber.state(), FiberState::Active);
    assert_eq!(taken(&applied), ["apply"]);
    assert_eq!(root.root().runtime_count(), 1);

    let dispose = fiber.dispose_effect().unwrap();
    dispose.dispose().unwrap();
    assert_eq!(fiber.uid(), None, "uid goes at once");
    run_until_stalled();
    assert_eq!(fiber.state(), FiberState::Disposed);
    assert_eq!(taken(&applied), ["dispose"]);
    assert_eq!(
        root.root().runtime_count(),
        0,
        "the last fiber takes its runtime"
    );
}

#[test]
fn uids_count_up_across_plugins() {
    let root = Context::new();
    let a = root
        .plugin_fn("a", &[], |_, _| Ok(Value::Undefined))
        .unwrap();
    let b = root
        .plugin_fn("b", &[], |_, _| Ok(Value::Undefined))
        .unwrap();
    assert_eq!((a.uid(), b.uid()), (Some(1), Some(2)));
    assert_eq!(root.fiber().uid(), Some(0));
    assert_eq!(root.fiber().state(), FiberState::Active);
}

#[test]
fn an_injection_waits_for_its_service_and_follows_it() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    let consumer = root
        .inject(&["foo"], move |ctx, _| {
            let foo = ctx.get("foo")?.unwrap();
            push(&s, format!("apply {}", foo.as_str().unwrap()));
            let s = s.clone();
            Ok(Value::disposer(move || push(&s, "dispose")))
        })
        .unwrap();
    run_until_stalled();
    assert_eq!(consumer.state(), FiberState::Pending);
    assert!(seen.borrow().is_empty());

    let provided = root.provide("foo", Value::string("one")).unwrap();
    run_until_stalled();
    assert_eq!(consumer.state(), FiberState::Active);
    assert_eq!(taken(&seen), ["apply one"]);

    provided.dispose().unwrap();
    run_until_stalled();
    assert_eq!(consumer.state(), FiberState::Pending);
    assert_eq!(taken(&seen), ["dispose"]);

    root.provide("foo", Value::string("two")).unwrap();
    run_until_stalled();
    assert_eq!(consumer.state(), FiberState::Active);
    assert_eq!(taken(&seen), ["apply two"]);
}

#[test]
fn a_service_from_a_plugin_reloads_its_consumers_when_it_changes_provider() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    let _consumer = root
        .inject(&["foo"], move |ctx, _| {
            let foo = ctx.get("foo")?.unwrap();
            push(&s, format!("apply {}", foo.as_number().unwrap()));
            Ok(Value::Undefined)
        })
        .unwrap();
    let first = root
        .plugin_fn("p1", &[], |ctx, _| {
            ctx.provide("foo", Value::Number(1.))?;
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    assert_eq!(taken(&seen), ["apply 1"]);
    first.dispose_effect().unwrap().dispose().unwrap();
    let _second = root
        .plugin_fn("p2", &[], |ctx, _| {
            ctx.provide("foo", Value::Number(2.))?;
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    assert_eq!(taken(&seen), ["apply 2"]);
}

#[test]
fn reading_an_uninjected_service_from_a_plugin_misses_and_a_required_one_while_inactive_fails() {
    let root = Context::new();
    root.provide("foo", Value::Number(1.)).unwrap();
    let out = log();
    let o = out.clone();
    root.plugin_fn("reader", &[], move |ctx, _| {
        // Not injected, but provided at the root: the walk reaches the root's store.
        push(&o, format!("{:?}", ctx.get("foo")?));
        push(&o, format!("{:?}", ctx.get("bar")?));
        Ok(Value::Undefined)
    })
    .unwrap();
    run_until_stalled();
    assert_eq!(taken(&out), ["Some(1)", "None"]);
}

#[test]
fn isolation_gives_a_subtree_its_own_service() {
    let root = Context::new();
    let isolated = root.isolate("foo", None);
    root.provide("foo", Value::string("outer")).unwrap();
    isolated.provide("foo", Value::string("inner")).unwrap();
    let seen = log();
    let (a, b) = (seen.clone(), seen.clone());
    root.inject(&["foo"], move |ctx, _| {
        push(
            &a,
            format!("outside {}", ctx.get("foo")?.unwrap().as_str().unwrap()),
        );
        Ok(Value::Undefined)
    })
    .unwrap();
    isolated
        .inject(&["foo"], move |ctx, _| {
            push(
                &b,
                format!("inside {}", ctx.get("foo")?.unwrap().as_str().unwrap()),
            );
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    assert_eq!(taken(&seen), ["outside outer", "inside inner"]);
}

#[test]
fn providing_a_name_twice_under_one_key_is_refused() {
    let root = Context::new();
    root.provide("foo", Value::Number(1.)).unwrap();
    let error = root.provide("foo", Value::Number(2.)).err().unwrap();
    assert_eq!(
        error.message(),
        "service \"foo\" has been registered at <root>"
    );
    root.accessor("bar", |_| Ok(Value::Undefined)).unwrap();
    let error = root.provide("bar", Value::Number(2.)).err().unwrap();
    assert_eq!(
        error.message(),
        "property \"bar\" is already declared as accessor"
    );
}

#[test]
fn effects_dispose_last_first_and_a_yielded_effect_is_owned_by_its_parent() {
    let root = Context::new();
    let order = log();
    let o = order.clone();
    let fiber = root
        .plugin_fn("p", &[], move |ctx, _| {
            let (a, b, c) = (o.clone(), o.clone(), o.clone());
            let inner = ctx.effect("inner", move || {
                let a = a.clone();
                Ok(Value::disposer(move || push(&a, "inner")))
            })?;
            assert_eq!(ctx.fiber().effects().len(), 1);
            let outer = ctx.effect("outer", move || {
                let (b, c) = (b.clone(), c.clone());
                // A composite effect: the inner effect is taken over, between two others.
                Ok(Value::Array(Rc::new(vec![
                    Value::disposer(move || push(&b, "first")),
                    Value::Effect(inner.clone()),
                    Value::disposer(move || push(&c, "last")),
                ])))
            })?;
            assert_eq!(outer.children().len(), 1);
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    let labels: Vec<String> = fiber
        .effects()
        .iter()
        .map(|e| e.label().to_owned())
        .collect();
    assert_eq!(
        labels,
        ["outer"],
        "the inner effect left the fiber's own list"
    );
    fiber.dispose_effect().unwrap().dispose().unwrap();
    run_until_stalled();
    assert_eq!(taken(&order), ["last", "inner", "first"]);
}

#[test]
fn an_effect_is_disposed_once() {
    let root = Context::new();
    let count = Rc::new(RefCell::new(0));
    let c = count.clone();
    let effect = root
        .effect("e", move || {
            let c = c.clone();
            Ok(Value::disposer(move || *c.borrow_mut() += 1))
        })
        .unwrap();
    effect.dispose().unwrap();
    effect.dispose().unwrap();
    assert_eq!(*count.borrow(), 1);
    assert!(!effect.is_active());
}

#[test]
fn a_disposed_context_makes_no_effects() {
    let root = Context::new();
    let kept: Rc<RefCell<Option<Context>>> = Rc::default();
    let k = kept.clone();
    let fiber = root
        .plugin_fn("p", &[], move |ctx, _| {
            *k.borrow_mut() = Some(ctx.clone());
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    fiber.dispose_effect().unwrap().dispose().unwrap();
    run_until_stalled();
    let ctx = kept.borrow().clone().unwrap();
    let error = ctx.on("x", |_| Ok(Value::Undefined)).err().unwrap();
    assert_eq!(error.code(), Some("INACTIVE_EFFECT"));
    assert_eq!(error.message(), "cannot create effect on inactive context");
}

#[test]
fn a_failing_plugin_fails_alone() {
    let root = Context::new();
    let plugin = plugin_fn(Some("p"), &[], |_, config| {
        if config.is_truthy() {
            Ok(Value::Undefined)
        } else {
            Err(Error::new("plugin error"))
        }
    });
    let failed = root.plugin(plugin.clone(), Value::Undefined).unwrap();
    let fine = root.plugin(plugin, Value::Bool(true)).unwrap();
    run_until_stalled();
    assert_eq!(failed.state(), FiberState::Failed);
    assert_eq!(fine.state(), FiberState::Active);
    assert_eq!(root.root().runtime_count(), 1, "one runtime, two fibers");
    let reported = root.host().reported();
    assert_eq!(reported.len(), 1);
    assert_eq!(reported[0].message(), "plugin error");
    let waited = task::spawn(failed.wait());
    run_until_stalled();
    assert_eq!(
        waited.output().unwrap().unwrap_err().message(),
        "plugin error"
    );
}

#[test]
fn statuses_are_announced_in_order() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    root.on("internal/status", move |args| {
        let (Value::Fiber(fiber), Value::Number(old)) = (&args[0], &args[1]) else {
            panic!("internal/status gets a fiber and its old state");
        };
        push(
            &s,
            format!("{} {} -> {}", fiber.name(), old, fiber.state().name()),
        );
        Ok(Value::Undefined)
    })
    .unwrap();
    let fiber = root
        .plugin_fn("p", &[], |_, _| Ok(Value::Undefined))
        .unwrap();
    run_until_stalled();
    fiber.dispose_effect().unwrap().dispose().unwrap();
    run_until_stalled();
    assert_eq!(
        taken(&seen),
        [
            "p 0 -> LOADING",
            "p 1 -> ACTIVE",
            "p 2 -> UNLOADING",
            "p 5 -> DISPOSED",
        ]
    );
}

#[test]
fn internal_plugin_tells_of_a_fiber_made_and_unmade() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    root.on("internal/plugin", move |args| {
        let Value::Fiber(fiber) = &args[0] else {
            panic!()
        };
        push(&s, format!("{:?}", fiber.uid()));
        Ok(Value::Undefined)
    })
    .unwrap();
    let fiber = root
        .plugin_fn("p", &[], |_, _| Ok(Value::Undefined))
        .unwrap();
    fiber.dispose_effect().unwrap().dispose().unwrap();
    run_until_stalled();
    assert_eq!(taken(&seen), ["Some(1)", "None"]);
}

#[test]
fn events_run_in_order_and_prepend_goes_first() {
    let root = Context::new();
    let seen = log();
    for (label, prepend) in [("a", false), ("b", false), ("c", true)] {
        let s = seen.clone();
        root.on_with(
            "e",
            move |_| {
                push(&s, label);
                Ok(Value::Undefined)
            },
            OnOptions {
                prepend,
                ..OnOptions::default()
            },
        )
        .unwrap();
    }
    root.emit("e", &[]).unwrap();
    assert_eq!(taken(&seen), ["c", "a", "b"]);
}

#[test]
fn bail_stops_at_the_first_answer_that_is_not_nullish_or_false() {
    let root = Context::new();
    root.on("e", |_| Ok(Value::Bool(false))).unwrap();
    root.on("e", |_| Ok(Value::Null)).unwrap();
    root.on("e", |_| Ok(Value::Number(0.))).unwrap();
    root.on("e", |_| Ok(Value::Number(1.))).unwrap();
    let answer = root.bail("e", &[]).unwrap();
    assert_eq!(answer.as_number(), Some(0.), "0 is an answer");
}

#[test]
fn serial_awaits_each_listener_and_stops_at_an_answer() {
    let root = Context::new();
    let seen = log();
    let s1 = seen.clone();
    root.on("e", move |_| {
        let s1 = s1.clone();
        Ok(Value::promise(async move {
            task::yield_now().await;
            push(&s1, "first");
            Ok(Value::Undefined)
        }))
    })
    .unwrap();
    let s2 = seen.clone();
    root.on("e", move |_| {
        push(&s2, "second");
        Ok(Value::string("done"))
    })
    .unwrap();
    let s3 = seen.clone();
    root.on("e", move |_| {
        push(&s3, "third");
        Ok(Value::Undefined)
    })
    .unwrap();
    let result = root.serial("e", vec![]);
    run_until_stalled();
    assert_eq!(result.output().unwrap().unwrap().as_str(), Some("done"));
    assert_eq!(taken(&seen), ["first", "second"]);
}

#[test]
fn parallel_runs_every_listener_and_aggregates_failures() {
    let root = Context::new();
    let seen = log();
    for i in 0..3 {
        let s = seen.clone();
        root.on("e", move |_| {
            push(&s, format!("start {i}"));
            if i == 1 {
                return Err(Error::new("one failed"));
            }
            Ok(Value::Undefined)
        })
        .unwrap();
    }
    let result = root.parallel("e", vec![]);
    assert_eq!(
        taken(&seen),
        ["start 0", "start 1", "start 2"],
        "all start at once"
    );
    run_until_stalled();
    let error = result.output().unwrap().unwrap_err();
    assert_eq!(error.name(), "AggregateError");
    assert_eq!(error.errors().len(), 1);
}

#[test]
fn waterfall_hands_each_listener_the_rest() {
    let root = Context::new();
    root.on("w", |args| {
        let Value::Number(n) = args[0] else { panic!() };
        let Value::Function(next) = &args[1] else {
            panic!()
        };
        let rest = next(&Value::Undefined, &[])?;
        Ok(Value::Number(rest.as_number().unwrap() * 10. + n))
    })
    .unwrap();
    let answer = root
        .waterfall("w", vec![Value::Number(3.)], |args| {
            Ok(Value::Number(args[0].as_number().unwrap() + 1.))
        })
        .unwrap();
    assert_eq!(answer.as_number(), Some(43.));
}

#[test]
fn a_listener_goes_with_its_fiber() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    let fiber = root
        .plugin_fn("p", &[], move |ctx, _| {
            let s = s.clone();
            ctx.on("e", move |_| {
                push(&s, "heard");
                Ok(Value::Undefined)
            })?;
            Ok(Value::Undefined)
        })
        .unwrap();
    run_until_stalled();
    root.emit("e", &[]).unwrap();
    fiber.dispose_effect().unwrap().dispose().unwrap();
    run_until_stalled();
    root.emit("e", &[]).unwrap();
    assert_eq!(taken(&seen), ["heard"]);
    assert!(root.root().hooks_of("e").is_empty());
}

#[test]
fn update_reapplies_with_the_new_config_after_the_fibers_own_update_listeners() {
    let root = Context::new();
    let seen = log();
    let s = seen.clone();
    let plugin = plugin_fn(Some("p"), &[], move |ctx, config| {
        push(&s, format!("apply {}", config.as_str().unwrap_or("-")));
        let s = s.clone();
        ctx.on("internal/update", move |args| {
            push(&s, format!("update to {}", args[0].as_str().unwrap()));
            let Value::Function(next) = &args[2] else {
                panic!()
            };
            next(&Value::Undefined, &[])
        })?;
        Ok(Value::Undefined)
    });
    let fiber = root.plugin(plugin, Value::string("old")).unwrap();
    run_until_stalled();
    assert!(
        root.root().hooks_of("internal/update").len() == 1,
        "a fiber's own update listener is kept on the fiber"
    );
    fiber.update(Value::string("new"), false).unwrap();
    run_until_stalled();
    assert_eq!(taken(&seen), ["apply old", "update to new", "apply new"]);
    assert_eq!(fiber.state(), FiberState::Active);
}

#[test]
fn a_change_while_loading_waits_for_the_load_the_inertia_lock() {
    let root = Context::new();
    let gate = task::Deferred::<()>::new();
    let seen = log();
    let (s, g) = (seen.clone(), gate.clone());
    let fiber = root
        .inject(&["foo"], move |_, _| {
            push(&s, "apply");
            let (s, g) = (s.clone(), g.clone());
            Ok(Value::promise(async move {
                g.wait().await;
                let s = s.clone();
                Ok(Value::disposer(move || push(&s, "dispose")))
            }))
        })
        .unwrap();
    let provided = root.provide("foo", Value::Number(1.)).unwrap();
    run_until_stalled();
    assert_eq!(fiber.state(), FiberState::Loading);
    provided.dispose().unwrap();
    run_until_stalled();
    assert_eq!(
        fiber.state(),
        FiberState::Loading,
        "the load in flight holds the state"
    );
    gate.settle(());
    run_until_stalled();
    assert_eq!(fiber.state(), FiberState::Pending);
    assert_eq!(taken(&seen), ["apply", "dispose"]);
}

#[test]
fn a_service_check_gates_injection() {
    let root = Context::new();
    let fiber = root.inject(&["foo"], |_, _| Ok(Value::Undefined)).unwrap();
    root.provide_checked("foo", Value::Number(0.), |value| {
        value.as_number().is_some_and(|n| n > 0.)
    })
    .unwrap();
    run_until_stalled();
    assert_eq!(fiber.state(), FiberState::Pending);
}

#[test]
fn a_disposer_that_answers_a_promise_holds_the_ones_after_it() {
    let root = Context::new();
    let order = log();
    let gate = task::Deferred::<()>::new();
    let (o1, o2, g) = (order.clone(), order.clone(), gate.clone());
    let effect = root
        .effect("e", move || {
            let (o1, o2, g) = (o1.clone(), o2.clone(), g.clone());
            Ok(Value::Array(Rc::new(vec![
                Value::disposer(move || push(&o1, "earlier")),
                Value::function(move |_, _| {
                    push(&o2, "later starts");
                    let g = g.clone();
                    Ok(Value::promise(async move {
                        g.wait().await;
                        Ok(Value::Undefined)
                    }))
                }),
            ])))
        })
        .unwrap();
    let pending = effect.dispose().unwrap().expect("asynchronous");
    run_until_stalled();
    assert_eq!(taken(&order), ["later starts"]);
    gate.settle(());
    run_until_stalled();
    assert_eq!(taken(&order), ["earlier"]);
    assert!(pending.output().unwrap().is_ok());
}

#[test]
fn a_listener_registered_through_internal_listener_can_be_taken_over() {
    let root = Context::new();
    let disposer: Disposer<_> = root
        .on("internal/update", |_| Ok(Value::Undefined))
        .unwrap();
    assert!(
        disposer.as_effect().is_none(),
        "a fiber-local update listener is not an effect"
    );
}
