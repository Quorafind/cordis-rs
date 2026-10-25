//! Disposing a plugin lets go of it: once nothing in JavaScript refers to its closures, the
//! engine does not either, so they are collected — what hot reload needs.
//!
//! Boa as Verve runs it never clears a `WeakRef` (kept objects are not released between jobs),
//! so collection is observed from Rust: each plugin captures a native-data object whose
//! finalizer counts. Every scenario captures that object in a different place the engine holds
//! (the plugin callback itself, a listener, an effect, a service, a child plugin, a fiber-local
//! update listener) and is driven to disposal a different way.

use std::cell::Cell;
use std::path::Path;

use boa_engine::object::{FunctionObjectBuilder, JsObject};
use boa_engine::{JsData, NativeFunction, js_string};
use boa_gc::{Finalize, Trace};

thread_local! {
    static COLLECTED: Cell<usize> = const { Cell::new(0) };
    /// Which run's probes count: an earlier runtime's may be finalized during a later run.
    static GENERATION: Cell<u32> = const { Cell::new(0) };
}

#[derive(Trace, JsData)]
struct Probe(#[unsafe_ignore_trace] u32);

impl Finalize for Probe {
    fn finalize(&self) {
        if GENERATION.with(Cell::get) == self.0 {
            COLLECTED.with(|collected| collected.set(collected.get() + 1));
        }
    }
}

const PLUGINS: usize = 100;

fn on_big_stack(f: impl FnOnce() + Send + 'static) {
    std::thread::Builder::new()
        .stack_size(64 << 20)
        .spawn(move || {
            verve::set_thread_stack(64 << 20);
            f();
        })
        .unwrap()
        .join()
        .unwrap();
}

fn settle(runtime: &mut verve::Runtime) {
    for _ in 0..20 {
        runtime.run_microtasks();
        runtime.run_due();
    }
}

/// Runs `iteration` `PLUGINS` times, each with a fresh `probe`; answers how many probes were
/// collected once all is disposed.
fn collected(native: bool, iteration: &str) -> usize {
    COLLECTED.with(|collected| collected.set(0));
    GENERATION.with(|generation| generation.set(generation.get() + 1));
    let mut runtime = verve::Runtime::new();
    let specifier = if native {
        cordis_verve::install(&mut runtime).unwrap();
        "cordis".to_owned()
    } else {
        let bundle = Path::new(env!("CARGO_MANIFEST_DIR"))
            .join("../../conformance/node_modules/cordis/index.js")
            .canonicalize()
            .unwrap();
        let text = bundle.to_string_lossy().replace('\\', "/");
        format!(
            "file:///{}",
            text.trim_start_matches("//?/").trim_start_matches('/')
        )
    };
    {
        let context = runtime.context();
        let make = FunctionObjectBuilder::new(
            context.realm(),
            NativeFunction::from_fn_ptr(|_, _, _| {
                let generation = GENERATION.with(Cell::get);
                Ok(JsObject::from_proto_and_data(None, Probe(generation)).into())
            }),
        )
        .build();
        context
            .global_object()
            .set(js_string!("makeProbe"), make, false, context)
            .unwrap();
    }
    let source = r#"
        import { Context, Service } from 'SPECIFIER';
        const root = new Context();
        globalThis.root = root;
        class Big extends Service {
            constructor(ctx) { super(ctx, 'big'); }
        }
        const tick = () => new Promise((resolve) => setTimeout(resolve, 0));
        for (let i = 0; i < PLUGINS; i++) {
            const probe = makeProbe();
            ITERATION
        }
        globalThis.size = root.registry.size;
    "#
    .replace("SPECIFIER", &specifier)
    .replace("PLUGINS", &PLUGINS.to_string())
    .replace("ITERATION", iteration);
    let main = Path::new(env!("CARGO_MANIFEST_DIR")).join("tests/release-main.mjs");
    let promise = runtime.load_esm(&main, &source).unwrap();
    for _ in 0..100_000 {
        if !matches!(
            promise.state(),
            boa_engine::builtins::promise::PromiseState::Pending
        ) {
            break;
        }
        settle(&mut runtime);
    }
    if let boa_engine::builtins::promise::PromiseState::Rejected(reason) = promise.state() {
        panic!("the script failed: {}", reason.display());
    }
    let size = runtime.eval("size").unwrap();
    assert_eq!(
        size.as_number(),
        Some(0.),
        "every runtime left the registry"
    );
    // Verve's collector frees lazily — a few dead objects per allocation — and what a native
    // handle holds is let go once it is freed and the next native call buries it; so each round
    // collects, allocates enough to free the last round's dead, and calls into the module.
    for _ in 0..12 {
        runtime.collect_garbage();
        runtime
            .eval("for (let i = 0; i < 2000; i++) ({ i }); root.registry.size;")
            .unwrap();
        settle(&mut runtime);
    }
    COLLECTED.with(Cell::get)
}

const SCENARIOS: &[&str] = &[
    // The callback itself, never loaded (its injection is missing).
    "const f = root.plugin({ inject: ['missing'], apply: (ctx) => { probe; } }); await tick(); await f.dispose();",
    // Disposed before its body ran; the body runs after (as upstream's reload does).
    "root.plugin((ctx) => { probe; }).dispose(); await tick();",
    // Loaded, then disposed.
    "const f = root.plugin((ctx) => { probe; }); await f; await f.dispose();",
    // Everything a plugin can register, holding the probe.
    "const f = root.plugin({ name: `p${i}`, apply(ctx) {
        ctx.on('tick', () => probe);
        ctx.effect(() => () => probe);
        ctx.provide(`svc${i}`, { probe });
        ctx.plugin(Big);
        ctx.on('internal/update', (config, noSave, next) => (probe, next()));
        ctx.inject(['big'], () => () => probe);
        return () => probe;
    } });
    await f; f.update({ again: true }); await f; await f.dispose();",
    // Failed while loading.
    "const f = root.plugin(() => { probe; throw new Error('no'); }); await tick(); await f.dispose();",
    // Removed through the registry.
    "const apply = (ctx) => { probe; }; await root.plugin(apply); root.registry.delete(apply); await tick();",
];

#[test]
fn a_disposed_plugin_is_collected() {
    on_big_stack(|| {
        let mut kept = Vec::new();
        for scenario in SCENARIOS {
            let native = collected(true, scenario);
            if native == PLUGINS {
                continue;
            }
            // A plugin that failed is kept by the root logger's buffer (its message holds a
            // `WeakRef` to the fiber, and Boa never clears one) — on the bundle the same.
            let vendored = collected(false, scenario);
            if native < vendored {
                kept.push(format!(
                    "{native}/{PLUGINS} collected (bundle: {vendored}) after: {scenario}"
                ));
            }
        }
        assert!(kept.is_empty(), "{}", kept.join("\n"));
        let live = cordis::stats::stats();
        assert_eq!(
            live.runtimes, 0,
            "no runtime outlives its plugins: {live:?}"
        );
        assert_eq!(live.pending_tasks, 0, "no task is left pending: {live:?}");
    });
}
