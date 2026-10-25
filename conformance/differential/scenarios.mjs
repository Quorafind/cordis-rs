// Differential scenarios: run once against the vendored cordis@4.0.0-rc.8 bundle and once
// against the native module, by `crates/cordis-harness/tests/differential.rs`. Each test
// records what it observes — events in the order they fire, fiber states at settle points
// and at microtask steps, errors by name and message, disposal order — and prints one
// `TRACE <name> <json>` line. The two runs must print the same lines.
import test from 'node:test';
import { Context, FiberState, Service } from 'cordis';

const sleep = (ms = 0) => new Promise((resolve) => setTimeout(resolve, ms));
const tick = () => Promise.resolve();
const ticks = async (n) => { for (let i = 0; i < n; i++) await tick(); };
const stateName = (state) => FiberState[state] ?? String(state);
const errorOf = (error) => (error ? { name: error.name, message: error.message, code: error.code } : null);

function scenario(name, body) {
  test(name, async () => {
    const trace = [];
    const log = (...entry) => trace.push(entry.length === 1 ? entry[0] : entry);
    const root = new Context();
    // Errors the framework reports, through the logger, as values.
    root.logger.exporter({
      export: (message) => {
        if (message.type !== 'error') return;
        const [first] = message.args;
        log('logged', first instanceof Error ? errorOf(first) : String(first));
      },
      levels: { default: 3 },
    });
    root.on('internal/status', (fiber, old) => log('status', fiber.name, stateName(old), stateName(fiber.state)));
    root.on('internal/plugin', (fiber) => log('plugin', fiber.name, fiber.uid === null ? 'gone' : 'made'));
    await body({ root, log });
    await sleep();
    console.log(`TRACE ${name} ${JSON.stringify(trace)}`);
  });
}

scenario('lifecycle of a plain plugin', async ({ root, log }) => {
  const fiber = root.plugin(function plain(ctx) {
    log('apply');
    return () => log('dispose');
  });
  log('sync state', stateName(fiber.state));
  for (let i = 0; i < 4; i++) {
    await tick();
    log('tick', i, stateName(fiber.state));
  }
  await fiber;
  log('awaited', stateName(fiber.state));
  await fiber.dispose();
  log('disposed', stateName(fiber.state), fiber.uid);
});

scenario('injection follows its provider', async ({ root, log }) => {
  const consumer = root.inject(['foo'], function consumer(ctx) {
    log('consume', ctx.foo.value);
    return () => log('release', ctx.foo.value);
  });
  await sleep();
  log('before', stateName(consumer.state));
  class Foo extends Service {
    constructor(ctx, config) {
      super(ctx, 'foo');
      this.value = config.value;
    }
  }
  const first = root.plugin(Foo, { value: 1 });
  await sleep();
  log('after first', stateName(consumer.state));
  await first.dispose();
  await sleep();
  log('after dispose', stateName(consumer.state));
  root.plugin(Foo, { value: 2 });
  await sleep();
  log('after second', stateName(consumer.state));
});

scenario('provide while loading: the inertia lock', async ({ root, log }) => {
  let release;
  const gate = new Promise((resolve) => { release = resolve; });
  const dispose = root.provide('foo', 1);
  const fiber = root.inject(['foo'], async function slow() {
    log('load start');
    await gate;
    log('load end');
    return () => log('unload');
  });
  await ticks(3);
  log('loading', stateName(fiber.state));
  dispose();
  await ticks(3);
  log('still', stateName(fiber.state));
  release();
  await sleep();
  log('settled', stateName(fiber.state));
});

scenario('effects dispose last-first, nested by identity', async ({ root, log }) => {
  const fiber = root.plugin(function nested(ctx) {
    const inner = ctx.effect(() => () => log('inner'), 'inner');
    ctx.effect(function* () {
      yield () => log('first');
      yield inner;
      yield () => log('last');
    }, 'outer');
    ctx.on('custom', () => {});
    log('effects', ctx.fiber.getEffects());
  });
  await fiber;
  await fiber.dispose();
});

scenario('async disposers chain', async ({ root, log }) => {
  const effect = root.effect(function* () {
    yield () => log('a');
    yield async () => { await sleep(); log('b'); };
    yield () => log('c');
  });
  const pending = effect();
  log('returned a promise', typeof pending?.then === 'function');
  await pending;
  log('done');
});

scenario('events: order, prepend, once, bail, serial, waterfall', async ({ root, log }) => {
  root.on('e', (x) => log('a', x));
  root.on('e', (x) => log('b', x));
  root.on('e', (x) => log('c', x), true);
  root.once('e', (x) => log('once', x));
  root.emit('e', 1);
  root.emit('e', 2);
  root.on('b', () => false);
  root.on('b', () => null);
  root.on('b', () => 0);
  root.on('b', () => 'late');
  log('bail', root.bail('b'));
  root.on('s', async () => { await tick(); log('s1'); });
  root.on('s', () => { log('s2'); return 'stop'; });
  root.on('s', () => log('s3'));
  log('serial', await root.serial('s'));
  root.on('w', (n, next) => next() * 10 + n);
  root.on('w', (n, next) => next() + 1);
  log('waterfall', root.waterfall('w', 3, (n) => n));
});

scenario('parallel aggregates failures', async ({ root, log }) => {
  root.on('p', () => log('p1'));
  root.on('p', async () => { await tick(); throw new Error('p2 failed'); });
  root.on('p', () => { throw new TypeError('p3 failed'); });
  try {
    await root.parallel('p');
  } catch (error) {
    log('rejected', error.constructor.name, error.errors.map(errorOf));
  }
});

scenario('filters and internal/dispatch', async ({ root, log }) => {
  root.on('internal/dispatch', (type, name, args) => log('dispatch', type, name, args.length));
  const a = root.extend({ tag: 'a' });
  const b = root.extend({ tag: 'b' });
  a.on('f', () => log('heard a'));
  b.on('f', () => log('heard b'));
  const session = { [Context.filter]: (ctx) => ctx.tag === 'b' };
  root.emit(session, 'f');
  root.emit('f');
  const args = [session, 'f', 42];
  const callbacks = root.events.dispatch('emit', args);
  log('dispatch left', args, callbacks.length);
});

scenario('a failing plugin fails alone and is reported', async ({ root, log }) => {
  const apply = (ctx, config) => {
    if (!config?.ok) throw new Error('plugin error');
  };
  const bad = root.plugin(apply);
  const good = root.plugin(apply, { ok: true });
  await sleep();
  log('states', stateName(bad.state), stateName(good.state), errorOf(bad._error));
  try {
    await bad;
  } catch (error) {
    log('await rejects', errorOf(error));
  }
});

scenario('errors from effects and disposers', async ({ root, log }) => {
  try {
    root.effect(() => 42);
  } catch (error) {
    log('invalid', errorOf(error));
  }
  const fiber = root.plugin(function throwsOnDispose() {
    return () => { throw new Error('dispose error'); };
  });
  await fiber;
  await fiber.dispose();
  await sleep();
  const dead = root.plugin(function dead(ctx) {});
  await dead;
  await dead.dispose();
  try {
    dead.ctx.on('x', () => {});
  } catch (error) {
    log('inactive', errorOf(error));
  }
});

scenario('update and restart', async ({ root, log }) => {
  const fiber = root.plugin(function configured(ctx, config) {
    log('apply', config);
    ctx.on('internal/update', (config, noSave, next) => {
      log('own update', config);
      return next();
    });
    return () => log('dispose', config);
  }, { v: 1 });
  await fiber;
  fiber.update({ v: 2 });
  await sleep();
  await fiber.restart();
  log('state', stateName(fiber.state));
});

scenario('isolation narrows a service to a subtree', async ({ root, log }) => {
  const label = Symbol('shared');
  const left = root.isolate('foo', label);
  const right = root.isolate('foo', label);
  root.provide('foo', 'outer');
  left.provide('foo', 'inner');
  root.inject(['foo'], (ctx) => log('root sees', ctx.foo));
  right.inject(['foo'], (ctx) => log('right sees', ctx.foo));
  await sleep();
  try {
    right.provide('foo', 'again');
  } catch (error) {
    log('second provide', errorOf(error));
  }
});

scenario('services: check, accessor, mixin, reflect', async ({ root, log }) => {
  class Counter extends Service {
    count = 0;
    constructor(ctx) { super(ctx, 'counter'); }
    increase() { return ++this.count; }
    [Service.check]() { return true; }
  }
  root.plugin(Counter);
  root.mixin('counter', ['increase']);
  root.accessor('answer', { get: () => 42 });
  root.inject(['counter'], (ctx) => {
    log('increase', ctx.increase(), ctx.counter.increase());
    log('answer', ctx.answer);
    try {
      log('missing', ctx.nothing);
    } catch (error) {
      log('missing threw', errorOf(error));
    }
  });
  await sleep();
  log('get', root.reflect.get('counter')?.count, 'has', 'answer' in root);
});

scenario('a service waits on its init before injectors load', async ({ root, log }) => {
  let ready;
  class Slow extends Service {
    constructor(ctx) { super(ctx, 'slow'); }
    async [Service.init]() {
      log('init start');
      await new Promise((resolve) => { ready = resolve; });
      log('init end');
    }
  }
  root.inject(['slow'], () => log('injected'));
  const fiber = root.plugin(Slow);
  await sleep();
  log('waiting', stateName(fiber.state));
  ready();
  await sleep();
  log('ready', stateName(fiber.state));
});

scenario('registry views', async ({ root, log }) => {
  function alpha() {}
  const one = root.plugin(alpha);
  const two = root.plugin(alpha);
  log('size', root.registry.size, 'has', root.registry.has(alpha));
  log('runtime', root.registry.get(alpha).name, [...root.registry.get(alpha).fibers].length);
  log('same runtime', one.runtime === two.runtime);
  await one.dispose();
  log('after one', root.registry.has(alpha));
  await two.dispose();
  log('after two', root.registry.has(alpha), root.registry.size);
});
