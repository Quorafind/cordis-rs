# What cordis-rs needs from Verve

Nothing here blocks cordis-rs: each item is worked around in `cordis-verve` (or is a
difference both the vendored bundle and the native module see). They are listed so Verve can
remove the workarounds, in order of how much they matter.

## 1. Untraced `JsValue`s dropped during a sweep stay rooted (Boa) — affects Verve's own natives

**What.** `boa_gc`'s `Drop for Gc` decrements the reference count only when
`finalizer_safe()`, i.e. not while the collector frees objects (`DropGuard`). That is right for
a traced `Gc` inside a freed object (its target is freed in the same sweep) and wrong for an
*untraced* one: a `JsValue` held by a native-data object (`#[unsafe_ignore_trace]`), or captured
by a `NativeFunction::from_closure` closure, is dropped during the sweep when its owner is freed,
the decrement is skipped, and the target counts as a root forever — with everything it reaches.

**Seen as.** Every disposed Cordis plugin stayed alive (its callback was held by a native
runtime object owned by a handle). Verve's own natives have the same exposure wherever a
closure or native data owns a `JsValue` (e.g. `native::closure` captures).

**Workaround.** `cordis-verve/src/sweep.rs`: handle contents and native-closure captures are
`SweepSafe` — dropped during a sweep they are parked in a thread-local graveyard and dropped at
the next native call (`sweep::bury`, from `cx::enter`). `tests/release.rs` proves plugins are
collected.

**What would remove it.** In `boa_gc`, queue the skipped decrements of `Gc`s dropped under the
`DropGuard` and apply them after the sweep for nodes that survived it; or a Verve-level
`SweepSafe` used by every native that owns `JsValue`s, with the graveyard buried once per turn.

## 2. `WeakRef` targets are never released

**What.** Boa keeps a `WeakRef`'s target alive until `ClearKeptObjects` (end of the job), which
Verve's executor never runs, so `new WeakRef(x).deref()` never becomes `undefined` and
`FinalizationRegistry` callbacks never fire.

**Seen as.** Cordis's logger puts `new WeakRef(fiber)` in every message it buffers (the root
logger keeps 1000), so a plugin that *failed* stays alive on Verve — on the vendored bundle and
the native module alike. Leak tests cannot use `WeakRef`, so `tests/release.rs` counts
finalizers of native probes instead.

**Wanted.** Call `Context::clear_kept_objects()` when the microtask queue drains (end of each
turn in `Runtime::run_microtasks`/the job executor).

## 3. A host's builtins with `verve::cli`

**What.** `verve::cli::main` makes its own `Runtime` and offers no hook to define builtins
before the script runs, and its event loop (`alive`, `has_ref_timers`, `network::keeps_alive`)
is private.

**Workaround.** `cordis-harness` (`cordis-run`) re-implements a loop over the public
`run_due`/`run_workers`/`next_due` API, driving until the test runner's promise settles.

**Wanted.** `cli::main_with(args, setup: impl FnOnce(&mut Runtime))`, or a public
`Runtime::keeps_alive()`.

## 4. Capturing and entering the async context natively

**What.** Engine continuations must resume inside the `AsyncLocalStorage` stores their task
was started in (compose-runtime attributes calls through them). The only public way is the JS
`AsyncLocalStorage.snapshot()`: one JS call per spawned task, and a JS call wrapping every later
poll.

**Wanted.** `verve::async_context::{capture(&Context) -> Snapshot, Snapshot::enter(&mut
Context, f)}` (the `hooks::current`/`within` pair, made public).

## 5. Error messages that name the callee

**What.** Calling a non-function throws `TypeError: not a callable function`; V8 says
`ctx.systemPrompt.tools is not a function`.

**Seen as.** compose-runtime's `a loop entry mounted outside its isolated group is refused`
matches `/systemPrompt|tools/` against that message, and fails on Verve with either Cordis.

## 6. `node:test`

Not provided; `cordis-run` brings a stand-in (`crates/cordis-harness/js/testkit.js`) for
`test`/`describe`/`it`, hooks and `mock.fn`, which is all compose-runtime's suites use.
