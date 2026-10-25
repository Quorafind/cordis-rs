//! A Rust plugin and JavaScript plugins in one context tree: one registry, one set of events,
//! one service table.

use std::cell::RefCell;
use std::path::Path;
use std::rc::Rc;

use boa_engine::{JsResult, JsString, JsValue};
use cordis_verve::{Ctx, RustPlugin, define_plugin};

struct Greeter {
    heard: Rc<RefCell<Vec<String>>>,
}

impl RustPlugin for Greeter {
    fn name(&self) -> &str {
        "greeter"
    }

    fn apply(&self, ctx: &Ctx, config: &JsValue) -> JsResult<JsValue> {
        let prefix = config
            .as_object()
            .map(|_| "configured")
            .unwrap_or("plain")
            .to_owned();
        let greet = ctx.function("greet", move |_, args| {
            let who = args
                .first()
                .and_then(JsValue::as_string)
                .map(|s| s.to_std_string_escaped())
                .unwrap_or_default();
            Ok(JsString::from(format!("hello {who} ({prefix})")).into())
        });
        let service = ctx.object(&[("greet", greet)])?;
        ctx.provide("greeter", service)?;
        let heard = self.heard.clone();
        let emitter = ctx.clone();
        ctx.on("ping", move |_, args| {
            let n = args.first().and_then(JsValue::as_number).unwrap_or(0.);
            heard.borrow_mut().push(format!("ping {n}"));
            emitter.emit("pong", &[JsValue::from(n + 1.)])?;
            Ok(JsValue::undefined())
        })?;
        let heard = self.heard.clone();
        Ok(ctx.function("dispose", move |_, _| {
            heard.borrow_mut().push("disposed".into());
            Ok(JsValue::undefined())
        }))
    }
}

fn run(source: &str, heard: Rc<RefCell<Vec<String>>>) -> verve::Runtime {
    let mut runtime = verve::Runtime::new();
    cordis_verve::install(&mut runtime).unwrap();
    define_plugin(
        &mut runtime,
        "cordis-rust:greeter",
        Rc::new(Greeter { heard }),
    )
    .unwrap();
    let promise = runtime
        .load_esm(Path::new("/interop/main.mjs"), source)
        .unwrap();
    for _ in 0..50 {
        runtime.run_microtasks();
        runtime.run_due();
    }
    if let boa_engine::builtins::promise::PromiseState::Rejected(reason) = promise.state() {
        panic!("the script failed: {}", reason.display());
    }
    runtime
}

fn global(runtime: &mut verve::Runtime, name: &str) -> String {
    let context = runtime.context();
    let value = context
        .global_object()
        .get(JsString::from(name), context)
        .unwrap();
    value.to_string(context).unwrap().to_std_string_escaped()
}

#[test]
fn a_rust_plugin_and_js_plugins_share_one_tree() {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(|| {
            verve::set_thread_stack(64 << 20);
            let heard = Rc::new(RefCell::new(Vec::new()));
            let mut runtime = run(
                r#"
                import { Context } from 'cordis';
                import greeter from 'cordis-rust:greeter';
                const root = new Context();
                const log = [];
                globalThis.log = log;
                root.on('pong', (n) => log.push(`pong ${n}`));
                root.inject(['greeter'], (ctx) => {
                    log.push(ctx.greeter.greet('js'));
                    log.push(`name ${ctx.fiber.name}`);
                });
                const fiber = root.plugin(greeter, { loud: true });
                await fiber;
                log.push(`rust plugin ${fiber.name} ${fiber.state}`);
                root.emit('ping', 1);
                await new Promise((r) => setTimeout(r, 0));
                await fiber.dispose();
                log.push(`after dispose ${root.registry.size}`);
                globalThis.done = log.join(' | ');
                "#,
                heard.clone(),
            );
            let done = global(&mut runtime, "done");
            assert!(done.contains("hello js (configured)"), "{done}");
            assert!(done.contains("rust plugin greeter 2"), "{done}");
            assert!(done.contains("pong 2"), "{done}");
            assert!(done.contains("after dispose 1"), "{done}");
            assert_eq!(*heard.borrow(), ["ping 1", "disposed"]);
        })
        .unwrap()
        .join()
        .unwrap();
}
