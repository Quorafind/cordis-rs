// A stand-in for `node:test` and for the part of vitest the Cordis specs use, on Verve.
//
// Evaluated as a script; its value is `{ nodeTest, vitest, run, filter }`. The host defines
// `nodeTest` as the `node:test` module and `vitest` as the `vitest` module, imports a test
// file (which declares its tests as it evaluates), then calls `run()`, which runs them in
// declaration order and answers `{ passed, failed, skipped }`.
//
// Fake timers follow @sinonjs/fake-timers as vitest uses it: the clock moves only when told
// to, and the async variants let the real event loop turn — so every microtask settles —
// before each timer fires.
(() => {
  'use strict';
  const realSetTimeout = globalThis.setTimeout;
  const realClearTimeout = globalThis.clearTimeout;
  const realSetInterval = globalThis.setInterval;
  const realClearInterval = globalThis.clearInterval;
  const realDateNow = Date.now;
  const inspect = (value) => {
    try {
      if (typeof value === 'string') return JSON.stringify(value);
      if (typeof value === 'function') return `[Function ${value.name || 'anonymous'}]`;
      if (typeof value === 'symbol') return value.toString();
      if (value instanceof Error) return `${value.name}: ${value.message}`;
      const seen = new WeakSet();
      return JSON.stringify(value, (key, item) => {
        if (typeof item === 'bigint') return `${item}n`;
        if (typeof item === 'function') return `[Function ${item.name || 'anonymous'}]`;
        if (typeof item === 'symbol') return item.toString();
        if (item && typeof item === 'object') {
          if (seen.has(item)) return '[Circular]';
          seen.add(item);
        }
        return item;
      }) ?? String(value);
    } catch {
      try { return String(value); } catch { return '[value]'; }
    }
  };

  // ─── Suites ──────────────────────────────────────────────────────────────────────────────

  const makeSuite = (name, parent) => ({
    name, parent, children: [], beforeAll: [], afterAll: [], beforeEach: [], afterEach: [], skip: false,
  });
  const root = makeSuite('', null);
  let current = root;

  function declareTest(name, options, fn, extra = {}) {
    if (typeof options === 'function') { fn = options; options = {}; }
    current.children.push({ kind: 'test', name: String(name), fn, options: options ?? {}, ...extra });
  }
  function declareSuite(name, fn, extra = {}) {
    const suite = makeSuite(String(name), current);
    Object.assign(suite, extra);
    current.children.push({ kind: 'suite', suite });
    const outer = current;
    current = suite;
    try { fn?.(); } finally { current = outer; }
  }

  const fullName = (suite, name) => {
    const parts = [name];
    for (let s = suite; s && s.parent; s = s.parent) parts.unshift(s.name);
    return parts.filter(Boolean).join(' > ');
  };

  const kit = { filter: undefined };

  function withTimeout(promise, ms, label) {
    let timer;
    const timeout = new Promise((_, reject) => {
      timer = realSetTimeout(() => reject(new Error(`timed out after ${ms}ms: ${label}`)), ms);
    });
    return Promise.race([promise, timeout]).finally(() => realClearTimeout(timer));
  }

  async function runSuite(suite, result, inheritedEach) {
    const each = {
      before: [...inheritedEach.before, ...suite.beforeEach],
      after: [...suite.afterEach, ...inheritedEach.after],
    };
    for (const hook of suite.beforeAll) await hook();
    for (const child of suite.children) {
      if (child.kind === 'suite') {
        if (child.suite.skip) continue;
        await runSuite(child.suite, result, each);
        continue;
      }
      const name = fullName(suite, child.name);
      if (kit.filter && !name.includes(kit.filter)) continue;
      if (child.skip || child.options.skip || child.todo) {
        result.skipped++;
        console.log(`skip - ${name}`);
        continue;
      }
      const started = realDateNow();
      try {
        for (const hook of each.before) await hook();
        const context = { name: child.name, fullName: name, mock: makeMockTracker() };
        const ms = child.options.timeout ?? 60000;
        await withTimeout(Promise.resolve().then(() => child.fn(context)), ms, name);
        for (const hook of each.after) await hook();
        result.passed++;
        console.log(`ok - ${name} (${realDateNow() - started}ms)`);
      } catch (error) {
        result.failed++;
        result.failures.push(name);
        const detail = error && error.stack ? error.stack : inspect(error);
        console.error(`not ok - ${name}\n  ${String(detail).split('\n').join('\n  ')}`);
      } finally {
        timers.uninstall();
      }
    }
    for (const hook of suite.afterAll) await hook();
  }

  kit.run = async function run() {
    const result = { passed: 0, failed: 0, skipped: 0, failures: [] };
    await runSuite(root, result, { before: [], after: [] });
    console.log(`# pass ${result.passed}, fail ${result.failed}, skip ${result.skipped}`);
    return result;
  };

  // ─── Mocks (node:test's `mock.fn`, vitest's `vi.fn`) ────────────────────────────────────

  // As node:test makes them: a Proxy of the implementation (a plain `function () {}` when
  // none is given), so whether it is constructible, its `prototype` and its `name` are the
  // implementation's own. Calls and constructions are both recorded.
  function mockFn(impl = function () {}, style = 'node') {
    const calls = [];
    const jestCalls = [];
    const results = [];
    let implementation = impl;
    const record = (thisArg, args, target, run) => {
      const call = { arguments: args, this: thisArg, target, result: undefined, error: undefined };
      calls.push(call);
      jestCalls.push(args);
      try {
        const value = run();
        call.result = value;
        results.push({ type: 'return', value });
        return value;
      } catch (error) {
        call.error = error;
        results.push({ type: 'throw', value: error });
        throw error;
      }
    };
    const context = style === 'node'
      ? {
        calls,
        callCount: () => calls.length,
        resetCalls: () => { calls.length = 0; },
        mockImplementation: (next) => { implementation = next; },
      }
      : { calls: jestCalls, results, get lastCall() { return jestCalls[jestCalls.length - 1]; } };
    const extras = {
      mock: context,
      mockImplementation: (next) => { implementation = next; return proxy; },
      mockReturnValue: (value) => { implementation = () => value; return proxy; },
      mockClear: () => { calls.length = 0; jestCalls.length = 0; results.length = 0; return proxy; },
    };
    const proxy = new Proxy(impl, {
      get(target, prop, receiver) {
        if (style === 'node' ? prop === 'mock' : Object.hasOwn(extras, prop)) return extras[prop];
        return Reflect.get(target, prop, receiver);
      },
      apply(target, thisArg, args) {
        return record(thisArg, args, undefined, () => Reflect.apply(implementation, thisArg, args));
      },
      construct(target, args, newTarget) {
        return record(undefined, args, newTarget, () => Reflect.construct(implementation, args, newTarget));
      },
    });
    return proxy;
  }

  function makeMockTracker() {
    return {
      fn: (impl) => mockFn(impl, 'node'),
      method(object, name, impl) {
        const original = object[name];
        const fn = mockFn(impl ?? original, 'node');
        object[name] = fn;
        fn.mock.restore = () => { object[name] = original; };
        return fn;
      },
      reset() {},
      restoreAll() {},
    };
  }

  // ─── Fake timers ─────────────────────────────────────────────────────────────────────────

  const timers = {
    installed: false,
    now: 0,
    nextId: 1,
    queue: new Map(),
    install(config = {}) {
      if (this.installed) this.uninstall();
      this.installed = true;
      const start = config.now instanceof Date ? config.now.getTime() : config.now ?? realDateNow();
      this.now = start;
      this.queue.clear();
      const add = (callback, delay, args, interval) => {
        const id = this.nextId++;
        const ms = Math.max(0, Number(delay) || 0);
        this.queue.set(id, { id, callback, args, callAt: this.now + ms, interval: interval ? Math.max(1, ms) : undefined });
        return { id, ref() { return this; }, unref() { return this; }, hasRef() { return true; }, [Symbol.toPrimitive]() { return id; } };
      };
      const clear = (handle) => {
        if (handle == null) return;
        const id = typeof handle === 'object' ? handle.id : Number(handle);
        this.queue.delete(id);
      };
      globalThis.setTimeout = (callback, delay, ...args) => add(callback, delay, args, false);
      globalThis.setInterval = (callback, delay, ...args) => add(callback, delay, args, true);
      globalThis.clearTimeout = clear;
      globalThis.clearInterval = clear;
      const self = this;
      Date.now = () => self.now;
    },
    uninstall() {
      if (!this.installed) return;
      this.installed = false;
      this.queue.clear();
      globalThis.setTimeout = realSetTimeout;
      globalThis.clearTimeout = realClearTimeout;
      globalThis.setInterval = realSetInterval;
      globalThis.clearInterval = realClearInterval;
      Date.now = realDateNow;
    },
    first(limit) {
      let best;
      for (const timer of this.queue.values()) {
        if (timer.callAt > limit) continue;
        if (!best || timer.callAt < best.callAt || (timer.callAt === best.callAt && timer.id < best.id)) best = timer;
      }
      return best;
    },
    fire(timer) {
      this.now = Math.max(this.now, timer.callAt);
      if (timer.interval) timer.callAt += timer.interval;
      else this.queue.delete(timer.id);
      if (typeof timer.callback === 'function') timer.callback(...timer.args);
    },
  };
  // A real turn of the event loop: every microtask queued so far settles first.
  const settle = () => new Promise((resolve) => realSetTimeout(resolve, 0));

  const vi = {
    fn: (impl) => mockFn(impl, 'jest'),
    useFakeTimers(config) { timers.install(config); return vi; },
    useRealTimers() { timers.uninstall(); return vi; },
    isFakeTimers: () => timers.installed,
    getTimerCount: () => timers.queue.size,
    advanceTimersByTime(ms) {
      const target = timers.now + ms;
      for (let timer; (timer = timers.first(target));) timers.fire(timer);
      timers.now = target;
      return vi;
    },
    async advanceTimersByTimeAsync(ms) {
      const target = timers.now + ms;
      for (;;) {
        await settle();
        const timer = timers.first(target);
        if (!timer) break;
        timers.fire(timer);
      }
      timers.now = target;
      await settle();
      return vi;
    },
    async runAllTimersAsync() {
      for (let i = 0; i < 10000; i++) {
        await settle();
        const timer = timers.first(Infinity);
        if (!timer) break;
        timers.fire(timer);
      }
      await settle();
      return vi;
    },
    runAllTimers() {
      for (let i = 0, timer; i < 10000 && (timer = timers.first(Infinity)); i++) timers.fire(timer);
      return vi;
    },
    async runOnlyPendingTimersAsync() { return vi.runAllTimersAsync(); },
    setSystemTime(now) { timers.now = now instanceof Date ? now.getTime() : now; },
  };

  // ─── expect: chai's BDD chains and jest's matchers, as far as the specs use them ─────────

  class AssertionError extends Error {
    constructor(message) { super(message); this.name = 'AssertionError'; }
  }

  function deepEqual(a, b, seen = new Map()) {
    if (Object.is(a, b)) return true;
    if (typeof a !== typeof b || a === null || b === null || typeof a !== 'object') {
      return false;
    }
    if (seen.get(a) === b) return true;
    seen.set(a, b);
    if (Object.getPrototypeOf(a) !== Object.getPrototypeOf(b)) {
      const plain = (v) => [Object.prototype, null].includes(Object.getPrototypeOf(v));
      if (!(plain(a) && plain(b))) return false;
    }
    if (Array.isArray(a)) {
      if (a.length !== b.length) return false;
      return a.every((item, i) => deepEqual(item, b[i], seen));
    }
    if (a instanceof Map) {
      if (a.size !== b.size) return false;
      for (const [key, value] of a) if (!b.has(key) || !deepEqual(value, b.get(key), seen)) return false;
      return true;
    }
    if (a instanceof Set) {
      if (a.size !== b.size) return false;
      for (const value of a) if (!b.has(value)) return false;
      return true;
    }
    if (a instanceof Date) return a.getTime() === b.getTime();
    if (a instanceof RegExp) return String(a) === String(b);
    const keysA = Object.keys(a).filter((k) => a[k] !== undefined || true);
    const keysB = Object.keys(b);
    if (keysA.length !== keysB.length) return false;
    return keysA.every((key) => Object.hasOwn(b, key) && deepEqual(a[key], b[key], seen));
  }

  function matchesError(error, expected) {
    if (expected === undefined) return true;
    if (typeof expected === 'string') return String(error?.message ?? error).includes(expected);
    if (expected instanceof RegExp) return expected.test(String(error?.message ?? error));
    if (typeof expected === 'function') return error instanceof expected;
    if (expected && typeof expected === 'object') {
      return Object.entries(expected).every(([key, value]) => deepEqual(error?.[key], value));
    }
    return false;
  }

  function lengthOf(value) {
    if (value instanceof Map || value instanceof Set) return value.size;
    return value?.length;
  }

  function makeAssertion(actual, flags = {}) {
    const check = (pass, message) => {
      if (flags.not ? pass : !pass) {
        throw new AssertionError(flags.not ? `expected not: ${message}` : `expected: ${message}`);
      }
    };
    const chain = {};
    const self = new Proxy(chain, {
      get(target, prop) {
        if (prop === 'then') return undefined;
        if (prop in target) return target[prop];
        if (typeof prop === 'string' && ['to', 'be', 'been', 'is', 'that', 'which', 'and', 'has', 'have', 'with', 'at', 'of', 'same', 'but', 'does', 'still', 'a', 'an'].includes(prop)) {
          return self;
        }
        return undefined;
      },
    });
    const define = (name, fn) => { chain[name] = fn; };
    const getter = (name, fn) => { Object.defineProperty(chain, name, { get: fn }); };
    getter('not', () => makeAssertion(actual, { ...flags, not: !flags.not }));
    getter('deep', () => makeAssertion(actual, { ...flags, deep: true }));
    getter('own', () => makeAssertion(actual, { ...flags, own: true }));
    getter('undefined', () => check(actual === undefined, `${inspect(actual)} to be undefined`));
    getter('null', () => check(actual === null, `${inspect(actual)} to be null`));
    getter('true', () => check(actual === true, `${inspect(actual)} to be true`));
    getter('false', () => check(actual === false, `${inspect(actual)} to be false`));
    getter('ok', () => check(!!actual, `${inspect(actual)} to be truthy`));
    getter('empty', () => check(lengthOf(actual) === 0 || (actual && typeof actual === 'object' && Object.keys(actual).length === 0), `${inspect(actual)} to be empty`));
    getter('resolves', () => makeAsync(actual, 'resolves', flags));
    getter('rejects', () => makeAsync(actual, 'rejects', flags));
    define('equal', (expected) => {
      if (flags.deep) check(deepEqual(actual, expected), `${inspect(actual)} to deeply equal ${inspect(expected)}`);
      else check(actual === expected, `${inspect(actual)} to equal ${inspect(expected)}`);
      return self;
    });
    define('equals', chain.equal);
    define('eql', (expected) => { check(deepEqual(actual, expected), `${inspect(actual)} to deeply equal ${inspect(expected)}`); return self; });
    define('length', (n) => { check(lengthOf(actual) === n, `${inspect(actual)} to have length ${n} (got ${lengthOf(actual)})`); return self; });
    define('lengthOf', chain.length);
    define('property', (name, ...value) => {
      const has = flags.own ? Object.hasOwn(Object(actual), name) : (actual != null && name in Object(actual));
      if (value.length) {
        check(has && (flags.deep ? deepEqual(actual[name], value[0]) : actual[name] === value[0]), `${inspect(actual)} to have property ${String(name)} of ${inspect(value[0])}`);
      } else {
        check(has, `${inspect(actual)} to have property ${String(name)}`);
      }
      return self;
    });
    define('instanceof', (ctor) => { check(actual instanceof ctor, `${inspect(actual)} to be an instance of ${ctor?.name}`); return self; });
    define('instanceOf', chain.instanceof);
    define('include', (item) => {
      let pass;
      if (typeof actual === 'string') pass = actual.includes(item);
      else if (Array.isArray(actual)) pass = flags.deep ? actual.some((x) => deepEqual(x, item)) : actual.includes(item);
      else if (actual instanceof Set || actual instanceof Map) pass = actual.has(item);
      else if (actual && typeof actual === 'object') pass = Object.entries(item).every(([k, v]) => deepEqual(actual[k], v));
      check(pass, `${inspect(actual)} to include ${inspect(item)}`);
      return self;
    });
    define('contain', chain.include);
    define('members', (items) => {
      const list = [...actual];
      check(list.length === items.length && items.every((item) => list.some((x) => deepEqual(x, item))), `${inspect(actual)} to have members ${inspect(items)}`);
      return self;
    });
    define('throw', (expected) => {
      let threw = false;
      let error;
      try { actual(); } catch (e) { threw = true; error = e; }
      if (flags.not) {
        if (threw && matchesError(error, expected)) {
          throw new AssertionError(`expected not to throw, threw ${inspect(error)}`);
        }
      } else if (!threw || !matchesError(error, expected)) {
        throw new AssertionError(threw ? `expected to throw ${inspect(expected)}, threw ${inspect(error)}` : 'expected to throw');
      }
      return self;
    });
    define('throws', chain.throw);
    define('a', (type) => { check(typeof actual === type || (type === 'array' && Array.isArray(actual)), `${inspect(actual)} to be a ${type}`); return self; });
    define('an', chain.a);
    define('above', (n) => { check(actual > n, `${inspect(actual)} to be above ${n}`); return self; });
    define('below', (n) => { check(actual < n, `${inspect(actual)} to be below ${n}`); return self; });
    // jest's matchers
    define('toBe', (expected) => { check(Object.is(actual, expected), `${inspect(actual)} to be ${inspect(expected)}`); });
    define('toEqual', (expected) => { check(deepEqual(actual, expected), `${inspect(actual)} to equal ${inspect(expected)}`); });
    define('toStrictEqual', chain.toEqual);
    define('toHaveLength', (n) => { check(lengthOf(actual) === n, `${inspect(actual)} to have length ${n} (got ${lengthOf(actual)})`); });
    define('toBeInstanceOf', (ctor) => { check(actual instanceof ctor, `${inspect(actual)} to be an instance of ${ctor?.name}`); });
    define('toBeUndefined', () => { check(actual === undefined, `${inspect(actual)} to be undefined`); });
    define('toBeDefined', () => { check(actual !== undefined, `${inspect(actual)} to be defined`); });
    define('toBeTruthy', () => { check(!!actual, `${inspect(actual)} to be truthy`); });
    define('toBeFalsy', () => { check(!actual, `${inspect(actual)} to be falsy`); });
    define('toContain', (item) => {
      const pass = typeof actual === 'string' ? actual.includes(item) : [...actual].includes(item);
      check(pass, `${inspect(actual)} to contain ${inspect(item)}`);
    });
    define('toThrow', (expected) => chain.throw(expected));
    define('toHaveBeenCalled', () => { check(actual.mock.calls.length > 0, 'mock to have been called'); });
    define('toHaveBeenCalledTimes', (n) => { check(actual.mock.calls.length === n, `mock to have been called ${n} times (got ${actual.mock.calls.length})`); });
    return self;
  }

  function makeAsync(promise, mode, flags) {
    const handler = {
      get(_, prop) {
        if (prop === 'then') return undefined;
        return (...args) => Promise.resolve(promise).then(
          (value) => {
            if (mode === 'rejects') throw new AssertionError(`expected promise to reject, resolved ${inspect(value)}`);
            return makeAssertion(value, flags)[prop](...args);
          },
          (error) => {
            if (mode === 'resolves') throw new AssertionError(`expected promise to resolve, rejected ${inspect(error)}`);
            if (prop === 'toThrow') {
              const pass = matchesError(error, args[0]);
              if (flags.not ? pass : !pass) throw new AssertionError(`expected rejection ${inspect(error)} to match ${inspect(args[0])}`);
              return;
            }
            return makeAssertion(error, flags)[prop](...args);
          },
        );
      },
    };
    return new Proxy({}, handler);
  }

  const expect = (actual) => makeAssertion(actual);

  // ─── The modules ─────────────────────────────────────────────────────────────────────────

  const it = (name, options, fn) => declareTest(name, options, fn);
  it.skip = (name, options, fn) => declareTest(name, options, fn, { skip: true });
  it.todo = (name) => declareTest(name, {}, () => {}, { todo: true });
  it.only = it;
  it.each = (rows) => (name, fn) => rows.forEach((row, i) => declareTest(`${name} [${i}]`, {}, () => (Array.isArray(row) ? fn(...row) : fn(row))));
  const describe = (name, fn) => declareSuite(name, fn);
  describe.skip = (name, fn) => declareSuite(name, fn, { skip: true });
  describe.only = describe;
  const hooks = {
    before: (fn) => current.beforeAll.push(fn),
    after: (fn) => current.afterAll.push(fn),
    beforeEach: (fn) => current.beforeEach.push(fn),
    afterEach: (fn) => current.afterEach.push(fn),
  };

  const nodeTest = function test(name, options, fn) { return declareTest(name, options, fn); };
  Object.assign(nodeTest, {
    test: nodeTest, it, describe, suite: describe,
    skip: it.skip, todo: it.todo, only: it,
    mock: makeMockTracker(),
    before: hooks.before, after: hooks.after,
    beforeEach: hooks.beforeEach, afterEach: hooks.afterEach,
  });

  const vitest = {
    describe, it, test: it, expect, vi,
    beforeAll: hooks.before, afterAll: hooks.after,
    beforeEach: hooks.beforeEach, afterEach: hooks.afterEach,
  };

  kit.nodeTest = nodeTest;
  kit.vitest = vitest;
  return kit;
})();
