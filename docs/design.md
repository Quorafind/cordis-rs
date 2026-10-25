# cordis-rs design

cordis-rs is Cordis — the meta-framework under Koishi — implemented in Rust, with a binding
that puts it on Verve as the `cordis` module so the vendored DeepSeek Harness packages run on
it unchanged. The reference is **cordis@4.0.0-rc.8** (tag `v4.0.0-rc.8`, `f46ae95`), the exact
build Rebon vendors: its semantics, its JavaScript surface, and its test suite.

## What the vendored packages actually need

Reading the DSH bundles and compose-runtime decides how much of Cordis's JavaScript object
model has to survive, and the answer is: all of it.

- `class X extends Service` with `super(ctx, name)`, `Service.init`, `[Service.tracker]`,
  `associate`; `FiberState`, `getTraceable`, `symbols.original`, `Logger`.
- `ctx.events.dispatch('emit', args)` (deprecated upstream, used five times), `ctx.serial`
  and `ctx.waterfall` read as functions and called detached.
- Effect disposers whose **identity** is load-bearing: a generator effect that yields the
  exact disposer of an inner `ctx.effect` nests that teardown (DSH's agent factory relies on
  it), so a disposer is a handle, not a callback copy.
- `ctx.extend({ [SINK]: … })` (compose's attribution rides on context prototype chains),
  `ctx.isolate(name, label)` with shared labels, `ctx.accessor`, `ctx.mixin`,
  `internal/status` listeners reading `fiber.state` and walking `fiber.parent.fiber`.
- Upstream's specs additionally check own-property shapes (`Object.hasOwn(fiber, 'state')`
  is false on the thenable `ctx.plugin()` returns), `ctx.events._hooks`, shadows, callable
  services (`Service.invoke`) and the logger's exporters.

So the JavaScript face must be Cordis's object model, prototype chains and proxies included,
while the machinery behind it is native.

## Layers

```
crates/cordis          pure Rust, no JS. The engine, generic over a Host.
crates/cordis-verve    Host = Boa/Verve values; `ops` (natives) + `js/cordis.js` (object model);
                       `install(&mut verve::Runtime)` defines the `cordis` builtin.
crates/cordis-harness  `cordis-run`: node:test/vitest stand-ins on Verve, for every JS suite.
conformance/           upstream specs (ported), differential scenarios, vendored reference.
```

### The core is generic over a `Host`

Cordis is a dynamically typed framework: configs, service values, event arguments and
listeners are whatever the host language has. Rather than invent a value type and convert at
every crossing, the engine is generic:

```rust
pub trait Host: Sized + 'static {
    type Value: Clone;          // configs, services, arguments, listeners, disposers
    type Ctx: Clone;            // a context, as the host represents one
    type Key: Clone + Eq + Hash;// an isolation label (a JS symbol; a minted id natively)
    type Error: Clone;
    type PluginKey: Clone + Eq + Hash; // registry identity of a plugin
    // calling, awaiting, classifying effect results, isolate lookups, deriving a fiber's
    // context, reporting errors (ctx.logger.error), capturing an async scope …
}
```

- `cordis::native::NativeHost` is the pure-Rust host: contexts are native `Context` nodes
  (fiber, isolation layer, intercept layer, parent), values are a small dynamic `Value`
  (`Rc<dyn Any>`, closures, futures, iterators), and typed helpers (`provide::<T>`,
  `get::<T>`, `on`, `plugin(impl Plugin)`) make Rust plugins and services first-class without
  any JS.
- `cordis_verve::JsHost` is Verve's: values are `JsValue`, contexts are the JS context objects,
  keys are `JsSymbol`s. A Rust plugin joins a JS tree as a native function (its `apply`), so it
  is registered, injected, reloaded and disposed by the same registry as JS plugins, and it
  drives the same engine through `cordis_verve::Ctx`.

### Data model

- **Root** (`Rc<RootInner>`): the registry (`PluginKey → Runtime`, insertion-ordered, as
  `Map` iteration is), the uid counter, the events table (`name → Vec<Hook>`), the reflect
  store (`Key → Rc<Impl>`), property declarations (`name → Service | Accessor{get,set}`), and
  the root fiber.
- **Runtime**: `name`, `callback`, `Config`, `fibers` (a disposable list, so iteration order
  and removal match `DisposableList`).
- **Fiber** (`Rc<FiberInner>`; `Weak` to its root): `uid: Option<u32>` (`None` = disposed),
  `state`, `epoch` (`Inactive` | a string of injected providers' uids, compared as upstream
  compares `_runner.epoch`), `inertia: Option<Task<()>>`, `store` (the snapshot taken at
  reload) and `_store` (checked impls), `inject` (ordered, nullable configs), `config`,
  `error`, `disposables` (ordered by serial number, removable by identity, cleared LIFO),
  local `internal/update` hooks, and the host-side context (`ctx`, `parent`).
- **Effect** (`Rc<EffectInner>`): label, children, epoch flag, the disposers it collected,
  the execution task. A JS effect wrapper is a function carrying the effect's handle, so a
  disposer handed back from JS is recognised as *that* effect — identity survives the crossing.
- **Hook**: `ctx`, `callback` (host value, or a native closure for the two built-in hooks the
  events service installs), `global`, `prepend`.

Every algorithm is a transcription of rc.8's: `_setEpoch`/`_updateState`/`_reload`/`_unload`
and their inertia lock, `effect` with function/promise/iterator/async-iterator results, the
dispose order (LIFO, a thenable result chaining the rest), `provide` → `notify` → `_checkImpl`
→ `_refresh`, `internal/update` as a waterfall over fiber-local hooks, the get trap's walk up
the fiber chain while the parent shares the root's isolation key, `bail`/`serial` bailing on
anything but `null`/`false`/`undefined`, `parallel` as `allSettled` + `AggregateError`.

### Async: JS `async function` semantics on Rust futures

Upstream is full of `async` functions whose *synchronous prefix* matters (`_reload` snapshots
the store before its first `await`; `_unload` clears the disposables synchronously; a
provider's disposer deletes and notifies before awaiting). The core reproduces that with
**eager tasks**: `task::spawn(future)` polls the future once, immediately, so it runs exactly
as far as a JS async function would before returning. Later polls happen when woken, from a
thread-local ready queue.

The queue is drained by whoever hosts the core:

- natively, `task::run_until_stalled()` (or any loop that calls it — a tokio `LocalSet`
  works the same way); a wake from another thread is queued and reported to a registered
  notifier so the owning loop can wake;
- on Verve, a drain is queued as a **microtask** (`Job::PromiseJob`) whenever the ready queue
  becomes non-empty, so core continuations interleave with JS promise reactions instead of
  waiting for a timer.

Awaiting a JS value (`H::await_value`) is `Promise.resolve(v).then(…)` with native reactions
that store the outcome and wake the task — the same number of hops `await` takes. A core task
returned to JS (`fiber.await()`, `ctx.serial(...)`) becomes a pending promise resolved when the
task finishes.

`AsyncLocalStorage` must survive: compose-runtime attributes calls through it. Each task
captures the host's async scope when it is spawned (`AsyncLocalStorage.snapshot()` on Verve)
and every later poll runs inside it — what a JS async function does with its context.

### Proxy semantics

`new Context()` returns `new Proxy(instance, handler)`, and every derived context
(`extend`, `isolate`, `intercept`, a fiber's own) is `Object.create` of it, exactly as rc.8 does,
so prototype-chained properties (`[SINK]`, `[Context.isolate]` layers) keep working. The
handler is the upstream one: special properties (symbols, `prototype`, `then`, numerics,
`_`-prefixed) pass through; own/inherited properties are traced (`getTraceable`); an accessor
declaration runs its getter; otherwise the `internal/get` waterfall ends in the native service
walk. `set` mirrors it through `internal/set` and `reflect.set`; `has` adds declared
properties. Traceable services (`[Service.tracker]`, `associate`, shadows, callable services)
are the upstream proxies verbatim — they are object-model, not state.

The split rule: **state and decisions are native** (which fiber is active, what is injected,
which hooks run in which order, what is disposed when, which service a name resolves to);
**object shapes are JavaScript** (proxies, prototype chains, `Service` subclassing, logger
formatting). `js/cordis.js` holds no state of its own beyond caches of JS objects the engine
asked it to make.

### Rooting and leaks (Boa GC vs Rust ownership)

Boa's collector treats a `Gc` pointer held outside the heap (in an `Rc`, in a Rust struct) as
a root. The engine holds JS values (contexts, listeners, configs, disposers, services), and JS
holds the engine (handles inside native-data objects), so a cycle through Rust is never
collected. The rule that keeps this bounded:

- **JS → native** edges are strong: a native-data `JsObject` owning an `Rc` handle (fiber,
  effect, root). They die with the JS object.
- **native → JS** edges are strong only while the owning native object is live in the
  Cordis sense, and are cut when it is disposed: a hook's callback and context when the hook
  is unregistered; an impl's value when the provider's effect is disposed; a fiber's context,
  parent context, callback, disposers and hooks when its unload finishes (`uid = null`). The
  JS wrapper keeps plain copies of what JS can still read from a dead fiber (`ctx`, `parent`,
  `runtime`, `inject`).
- **native → native** upward edges are weak (fiber → root), downward strong (root → registry
  → fibers). Where upstream leaves a cycle for the collector (a deleted runtime still listing
  its fibers; a runtime face re-made after deletion), the engine cuts it explicitly.
- **Drops during a sweep are deferred.** Boa skips the reference-count decrement of a `Gc`
  dropped while it frees objects; a `JsValue` owned by a freed handle or by a freed native
  closure's captures would therefore stay rooted forever. Both are wrapped in `SweepSafe`
  (`cordis-verve/src/sweep.rs`), which parks such contents and drops them at the next native
  call. See `verve-needs.md` §1.

So unloading a plugin releases its code and closures — what hot reload needs;
`cordis-verve/tests/release.rs` checks it six ways. What is not collected is a **root
abandoned with live fibers**: the root holds its root context, which holds the root (about one
fiber and the ~20 built-in effects of the root, per root). Rebon keeps one realm per process;
tests that make many roots leak that much each. A GC-traced store (keeping engine-held JS values
inside a traced `JsObject`) would remove it, and is noted as follow-up. `cordis::stats::stats()`
reports live fibers, effects, runtimes and pending tasks.

Every native entry (an op, a promise reaction, a drain job) records the `&mut boa::Context` it
was given in a thread-local stack for the duration of the call, which is how `JsHost` reaches
the engine from inside core code; the engine never holds a `RefCell` borrow across a host call,
so re-entrancy (a listener that emits, a disposer that provides) is safe.

### cosmokit and schemastery stay JavaScript

Neither is part of Cordis's semantics. The core touches config validation only through the
Standard Schema `~standard.validate` call (`resolveConfig`), which the host performs — a
schemastery `Schema` is just a value with that method. cosmokit is a utility library the DSH
packages import directly. Porting either would change nothing observable and is not needed for
the native `cordis` module, which does not import them at all.

## In scope for v1

- The whole rc.8 core: `Context` (+ `extend`/`isolate`/`intercept`, the proxy), `Fiber`
  (states, epochs, inertia, `effect`, `await`, `restart`, `update`, `getEffects`),
  `RegistryService`, `EventsService` (`on`/`once`/`emit`/`parallel`/`serial`/`bail`/
  `waterfall`/`dispatch`, `internal/*`), `ReflectService` (`provide`/`accessor`/`mixin`/
  `get`/`set`/`notify`/`trace`/`bind`), `Service`, `Inject` (array, object, decorators),
  `LoggerService`/`Logger`, `CordisError`, `ValidationError`, and every export of the bundle.
- The conformance suite (upstream specs on both implementations), Rust tests for the core
  on `NativeHost`, and a differential test comparing event order, states and errors.
- compose-runtime's composition, agent-loop and dsh-tools suites on the native module.

Not in v1: rewriting stack traces (`composeError` keeps errors as thrown; upstream only
splices stack lines), the loader/include/hmr packages, and a GC-traced store for abandoned
roots.

## Known differences from the bundle

None is observed by the upstream specs, the differential scenarios or compose-runtime's suites.

- **Views, not live objects.** `ctx.events._hooks`, `ctx.reflect.store`, `fiber.store`,
  `fiber._store`, `fiber._hooks` and `fiber._disposables` are snapshots, and
  `ctx.reflect.props` a read-only live view: mutating them changes nothing. Upstream's are the
  internal tables themselves.
- **`reflect.notify(names, filter)`** ignores a custom `filter` (the default — same isolation
  key — always applies). `EventsService.register` (an internal helper) is not provided.
- **Effect metadata** (`wrapper[Context.effect]`, `getEffects()`) is built fresh on each read:
  equal, not identical, across reads.
- **Prepending a fiber-local `internal/update` listener** works; upstream calls the missing
  `DisposableList.unshift` and throws.
- **Microtask interleaving.** Engine continuations run in drain microtasks queued when a task
  is woken, so the number of ticks between two engine steps can differ from upstream's `await`
  chains. Every ordering the differential scenarios observe (including state at single-tick
  steps) is identical; an observer counting ticks inside a load could see a difference.
- **Stack traces** are the engine's as thrown (see above); messages, names and codes match.
