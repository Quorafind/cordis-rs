# Putting the native Cordis into Rebon

Four changes, none of them to the vendored payload. Verified by
`cargo test -p cordis-harness --test conformance -- --ignored`, which applies the resolver change
below to a copy of `runtimes/node` and replaces the vendored bundle with a module that throws:
compose-runtime's composition, agent-loop, dsh-tools, llm-turn and own-schedule-invoke suites
end exactly as they do on the bundle.

## 1. Dependency

`cordis-verve` must see the same `verve` crate as `rebon-verve-host`, or `verve::Runtime` is
two different types. cordis-rs takes Verve by path (`../verve`) for development; for Rebon,
give its workspace the source Rebon uses, in cordis-rs's root `Cargo.toml`:

```toml
[workspace.dependencies]
verve = { git = "https://github.com/Quorafind/verve.git", branch = "main", default-features = false }
```

(and drop cordis-rs's `[patch.crates-io]` only if building it standalone against git — Rebon's
own `[patch.crates-io]` already points every `boa_*` crate at Verve's tree, which is what
`cordis-verve`'s `boa_engine`/`boa_gc = "=0.22.0"` need.) Then in Rebon's root `Cargo.toml`:

```toml
[workspace.dependencies]
cordis-verve = { path = "../cordis-rs/crates/cordis-verve" }   # or git, once it has a remote
```

and in `crates/rebon-verve-host/Cargo.toml`: `cordis-verve.workspace = true`.

## 2. Install it on the host's runtime

`crates/rebon-verve-host/src/guest.rs`, `HostGuest::boot`, after the permissions are set and
before the entry is loaded:

```rust
cordis_verve::install(&mut self.runtime)?;   // defines the `cordis` builtin
let promise = self.runtime.load_esm(&entry, &source)?;
```

`install` runs on the isolate's thread (it must: the engine's tasks and the runtime are
thread-local), defines `cordis` as a builtin module (so also `node:cordis`), and is idempotent.
One isolate per thread is what Rebon already does, and what the engine requires: a thread must
not interleave two cordis-enabled runtimes (`design.md`, Constraints).

## 3. Module resolution

compose-runtime's resolve hook maps `cordis` and `@deepseek-ai/cordis` to the vendored bundle
and short-circuits, so it has to be told about the builtin. In
`runtimes/node/compose-runtime/src/resolve.mjs`:

```js
import { isBuiltin, registerHooks } from 'node:module';
// …
  registerHooks({
    resolve(specifier, context, next) {
      // The native Cordis, when the host defines it (cordis-rs on Verve).
      if ((specifier === 'cordis' || specifier === '@deepseek-ai/cordis') && isBuiltin('cordis')) {
        return { url: 'node:cordis', shortCircuit: true };
      }
      const mapped = state.map.get(specifier);
      // …
```

On Node (`node --test` in CI) `isBuiltin('cordis')` is false, and the bundle is used as
before. `test/resolve.test.mjs` is unaffected (it compares the map, which is unchanged).

## 4. Manifests and payload

Nothing changes. `payload-manifests.json`'s `"cordis": { "entry": "vendor/cordis/index.js" }`
and `provided-modules.json` stay: the bundle remains the payload sentinel (`payload.mjs`), the
Node fallback, and the reference cordis-rs's differential tests run against. Removing it later
means changing `SENTINEL` in `payload.mjs` and dropping the two `provided-modules.json` entries.

## Build

No build step: `js/cordis.js` is compiled in with `include_str!`. `CORDIS_VERVE_JS=<path>`
substitutes a file for it at runtime, for working on it without rebuilding.

## Rust plugins in the composition

`cordis_verve::define_plugin(&mut runtime, "cordis-rust:name", Rc::new(MyPlugin))` (next to
`install`) makes a `RustPlugin` importable. compose-runtime's loader imports an entry *file*, so
the entry is a one-line module, `export * from 'cordis-rust:name';` — its namespace has the
plugin's own `apply`, `name` and `inject`, and `cordisPluginOf` takes it for a Cordis plugin. It
needs a `payload-manifests.json` entry like any other package.
