# cordis-rs

[Cordis](https://github.com/cordiverse/cordis) — the meta-framework under Koishi — in Rust,
and on [Verve](../verve) as the `cordis` module, so the DeepSeek Harness packages Rebon composes
run on a native engine instead of the vendored JavaScript bundle. The reference is
**cordis@4.0.0-rc.8**, the exact build Rebon vendors.

```
crates/cordis           the engine: contexts, registry, fibers, effects, events, services.
                        Pure Rust, generic over a Host; `cordis::native` is the host for Rust.
crates/cordis-verve     the `cordis` module on Verve: JsHost, native ops, js/cordis.js (the
                        object model), and RustPlugin for Rust plugins in a JS tree.
crates/cordis-harness   cordis-run: node:test / vitest stand-ins on Verve; the conformance tests.
conformance/upstream    cordis@4.0.0-rc.8's specs, ported to plain modules (tools/port-specs.cjs).
conformance/differential  scenarios whose traces must match on the bundle and natively.
conformance/bench       hot-path timings on both.
conformance/node_modules  the published bundle (and cosmokit, schemastery): the reference.
docs/                   design.md, integration.md (into Rebon), verve-needs.md.
```

## Status

- **Upstream specs:** 71/71 pass on the native module (and on the bundle, under the same
  runner).
- **Differential:** 16 scenarios — lifecycle states at single-microtask steps, inertia lock,
  effect nesting by identity, async disposer chains, every dispatch mode, filters and
  `internal/dispatch`, failures and their reports, update/restart, isolation, services and
  accessors, `Service.init`, registry views, the export list — identical traces.
- **Rebon's compose-runtime:** composition 10/10, dsh-tools 5/5, llm-turn 5/5,
  own-schedule-invoke 5/5, agent-loop 2/3 — test for test what the bundle gives on Verve (the
  agent-loop failure is a Boa error message, `verve-needs.md` §5).
- **Rust:** 24 engine tests on `NativeHost`; a Rust plugin and JS plugins sharing one tree;
  disposed plugins collected (six ways of disposing).
- **Speed** (debug engine, Boa at opt-level 3): the native module is faster on every hot path
  measured — emit 1.25×, plugin load+dispose 3.6×, service access 1.4×, provide/withdraw with
  50 injectors 12×.

## Checking it

```sh
cargo test                                   # engine, binding, conformance, differential
cargo test -p cordis-harness --test conformance -- --ignored   # compose-runtime, natively
target/debug/cordis-run --native conformance/upstream/fiber.spec.mjs
```

The ignored test reads Rebon's `runtimes/node` (`CORDIS_REBON_RUNTIMES`, default
`../reboncode/runtimes/node`) into a temporary copy; the checkout is never written.

## License

MIT. Cordis's semantics, API and tests are Shigma's, MIT; see `NOTICE`.
