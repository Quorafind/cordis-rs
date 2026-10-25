// The `cordis` module on Verve: Cordis 4.0.0-rc.8's JavaScript surface over the native engine.
//
// Evaluated as one function expression, called by the binding with `N` (the native ops) and
// `K` (the private symbols the engine reads: a face's handle, a context's root, a fiber face's
// context). It answers the module's exports.
//
// What lives here is object model, kept as upstream writes it: the `Context` proxy and its
// prototype chains, traceable services and shadows, `Service`, `Logger`, `Inject`. What a
// context, fiber, effect or event *does* — which fiber is active, what is injected, which
// listeners run in what order, what is disposed when, which service a name resolves to — is
// the engine's, reached through `N`. Nothing here keeps state of its own.
//
// Derived from Cordis (https://github.com/cordiverse/cordis), MIT, Copyright (c) 2021-present
// Shigma.
(function (N, K) {
  'use strict';

  const kHandle = K.handle;
  const kRoot = K.root;
  const kCtx = K.ctx;
  const kParent = K.parent;

  const defineProperty = (object, key, value) => Object.defineProperty(object, key, {
    writable: true, value, enumerable: false,
  });
  const isNullable = (value) => value === null || value === undefined;
  const hyphenate = (source) => source.replace(/[A-Z]/g, (char) => '-' + char.toLowerCase());

  // ─── utils.ts ──────────────────────────────────────────────────────────────────────────

  class DisposableList {
    sn = 0;
    map = new Map();
    weak = new WeakMap();
    get length() { return this.map.size; }
    push(value) {
      const sn = ++this.sn;
      this.map.set(sn, value);
      this.weak.set(value, sn);
      return () => this.map.delete(sn);
    }
    delete(value) {
      const sn = this.weak.get(value);
      if (!sn) return false;
      return this.map.delete(sn);
    }
    clear() {
      const values = [...this.map.values()];
      this.map.clear();
      return values.reverse();
    }
    [Symbol.iterator]() { return this.map.values(); }
    [Symbol.for('nodejs.util.inspect.custom')]() { return [...this]; }
  }

  const symbols = {
    shadow: Symbol.for('cordis.shadow'),
    caller: Symbol.for('cordis.caller'),
    receiver: Symbol.for('cordis.receiver'),
    original: Symbol.for('cordis.original'),
    metadata: Symbol.for('cordis.metadata'),
    initHooks: Symbol.for('cordis.initHooks'),
    checkProto: Symbol.for('cordis.checkProto'),
    effect: Symbol.for('cordis.effect'),
    filter: Symbol.for('cordis.filter'),
    isolate: Symbol.for('cordis.isolate'),
    intercept: Symbol.for('cordis.intercept'),
    init: Symbol.for('cordis.init'),
    check: Symbol.for('cordis.check'),
    config: Symbol.for('cordis.config'),
    invoke: Symbol.for('cordis.invoke'),
    extend: Symbol.for('cordis.extend'),
    tracker: Symbol.for('cordis.tracker'),
    resolveConfig: Symbol.for('cordis.resolveConfig'),
  };

  const GeneratorFunction = function* () {}.constructor;
  const AsyncGeneratorFunction = async function* () {}.constructor;

  function isConstructor(func) {
    if (!func.prototype) return false;
    if (func instanceof GeneratorFunction) return false;
    if (AsyncGeneratorFunction !== Function && func instanceof AsyncGeneratorFunction) return false;
    return true;
  }

  function joinPrototype(proto1, proto2) {
    if (proto1 === Object.prototype) return proto2;
    const result = Object.create(joinPrototype(Object.getPrototypeOf(proto1), proto2));
    for (const key of Reflect.ownKeys(proto1)) {
      Object.defineProperty(result, key, Object.getOwnPropertyDescriptor(proto1, key));
    }
    return result;
  }

  function isObject(value) {
    return value && (typeof value === 'object' || typeof value === 'function');
  }

  function getPropertyDescriptor(target, prop) {
    let proto = target;
    while (proto) {
      const desc = Reflect.getOwnPropertyDescriptor(proto, prop);
      if (desc) return desc;
      proto = Object.getPrototypeOf(proto);
    }
  }

  function getTraceable(ctx, value) {
    if (!isObject(value)) return value;
    if (Object.hasOwn(value, symbols.shadow)) {
      return Object.getPrototypeOf(value);
    }
    const tracker = value[symbols.tracker];
    if (!tracker) return value;
    return createTraceable(ctx, value, tracker);
  }

  function withProps(target, props) {
    if (!props) return target;
    return new Proxy(target, {
      get: (target2, prop, receiver) => {
        if (prop in props && prop !== 'constructor') return Reflect.get(props, prop, receiver);
        return Reflect.get(target2, prop, receiver);
      },
      set: (target2, prop, value, receiver) => {
        if (prop in props && prop !== 'constructor') return Reflect.set(props, prop, value, receiver);
        return Reflect.set(target2, prop, value, receiver);
      },
    });
  }

  function withProp(target, prop, value) {
    return withProps(target, Object.defineProperty(Object.create(null), prop, {
      value,
      writable: false,
    }));
  }

  function createShadow(ctx, target, property, receiver) {
    if (!property) return receiver;
    const origin = getPropertyDescriptor(target, property)?.value;
    if (!origin) return receiver;
    return withProp(receiver, property, ctx.extend({ [symbols.shadow]: origin }));
  }

  function createShadowMethod(ctx, value, outer, shadow) {
    return new Proxy(value, {
      apply: (target, thisArg, args) => {
        if (thisArg === outer) thisArg = shadow;
        return getTraceable(ctx, Reflect.apply(target, thisArg, args));
      },
    });
  }

  const hasProp = (ctx, name) => N.hasProp(ctx[kRoot], name);

  function createTraceable(ctx, value, tracker) {
    const caller = ctx[symbols.shadow] ?? ctx;
    if (ctx[symbols.shadow]) {
      ctx = Object.getPrototypeOf(ctx);
    }
    const proxy = new Proxy(value, {
      get: (target, prop, receiver) => {
        if (prop === symbols.original) return target;
        if (prop === symbols.caller) return caller;
        if (prop === tracker.property) return ctx;
        if (typeof prop === 'symbol') {
          return Reflect.get(target, prop, receiver);
        }
        if (tracker.associate && hasProp(ctx, `${tracker.associate}.${prop}`)) {
          return Reflect.get(ctx, `${tracker.associate}.${prop}`, withProp(ctx, symbols.receiver, receiver));
        }
        let shadow, innerValue;
        const desc = getPropertyDescriptor(target, prop);
        if (desc && 'value' in desc) {
          innerValue = desc.value;
        } else {
          shadow = createShadow(ctx, target, tracker.property, receiver);
          innerValue = Reflect.get(target, prop, shadow);
        }
        const innerTracker = innerValue?.[symbols.tracker];
        if (innerTracker) {
          return createTraceable(ctx, innerValue, innerTracker);
        } else if (!tracker.noShadow && typeof innerValue === 'function') {
          shadow ??= createShadow(ctx, target, tracker.property, receiver);
          return createShadowMethod(ctx, innerValue, receiver, shadow);
        } else {
          return innerValue;
        }
      },
      set: (target, prop, value2, receiver) => {
        if (prop === symbols.original) return false;
        if (prop === symbols.caller) return false;
        if (prop === tracker.property) return false;
        if (typeof prop === 'symbol') {
          return Reflect.set(target, prop, value2, receiver);
        }
        if (tracker.associate && hasProp(ctx, `${tracker.associate}.${prop}`)) {
          return Reflect.set(ctx, `${tracker.associate}.${prop}`, value2, withProp(ctx, symbols.receiver, receiver));
        }
        const shadow = createShadow(ctx, target, tracker.property, receiver);
        return Reflect.set(target, prop, value2, shadow);
      },
      apply: (target, thisArg, args) => {
        const receiver = tracker.noShadow ? proxy : createShadow(ctx, target, tracker.property, proxy);
        return applyTraceable(receiver, target, thisArg, args);
      },
    });
    return proxy;
  }

  function applyTraceable(proxy, value, thisArg, args) {
    if (!value[symbols.invoke]) return Reflect.apply(value, thisArg, args);
    return value[symbols.invoke].apply(proxy, args);
  }

  function createCallable(name, proto, tracker) {
    const self = function (...args) {
      const proxy = createTraceable(self['ctx'], self, tracker);
      return Reflect.apply(proxy, this, args);
    };
    defineProperty(self, 'name', name);
    return Object.setPrototypeOf(self, proto);
  }

  // Upstream splices stack lines so an error points at the plugin rather than the framework.
  // The engine reports errors as thrown; these keep the exported signatures and the one
  // semantic part — a thrown non-error becomes an `Error`.
  function composeError(callback, getOuterStack = buildOuterStack()) {
    const info = { offset: 1, error: undefined };
    const handle = (reason) => {
      if (typeof reason?.stack !== 'string') throw new Error(reason);
      throw reason;
    };
    try {
      const result = callback(info);
      if (isObject(result) && 'then' in result) return result.then(undefined, handle);
      return result;
    } catch (reason) {
      handle(reason);
    }
  }

  function buildOuterStack(offset = 0) {
    const outerError = new Error();
    return () => String(outerError.stack ?? '').split('\n').slice(3 + offset);
  }

  // ─── fiber.ts ──────────────────────────────────────────────────────────────────────────

  const kValidationError = Symbol.for('ValidationError');

  class ValidationError extends TypeError {
    name = 'ValidationError';
    constructor(issues) {
      super('invalid config:\n' + issues.map((issue) => {
        if (issue.path) {
          return `  - ${issue.message} (at ${issue.path.join('.')})`;
        } else {
          return `  - ${issue.message}`;
        }
      }).join('\n'));
    }
  }
  Object.defineProperty(ValidationError.prototype, kValidationError, { value: true });

  function resolveConfig(runtime, config) {
    if (!runtime.Config) return config;
    const result = runtime.Config['~standard'].validate(config);
    if ('then' in result) {
      throw new TypeError('Async config validation is not supported');
    }
    if (result.issues) {
      throw new ValidationError(result.issues);
    } else {
      return result.value;
    }
  }

  const FiberState = ((FiberState2) => {
    FiberState2[FiberState2['PENDING'] = 0] = 'PENDING';
    FiberState2[FiberState2['LOADING'] = 1] = 'LOADING';
    FiberState2[FiberState2['ACTIVE'] = 2] = 'ACTIVE';
    FiberState2[FiberState2['FAILED'] = 3] = 'FAILED';
    FiberState2[FiberState2['DISPOSED'] = 4] = 'DISPOSED';
    FiberState2[FiberState2['UNLOADING'] = 5] = 'UNLOADING';
    return FiberState2;
  })({});

  class CordisError extends Error {
    constructor(code, message) {
      super(message ?? CordisError.Code[code]);
      this.code = code;
    }
    code;
  }
  CordisError.Code = {
    INACTIVE_EFFECT: 'cannot create effect on inactive context',
  };

  // A fiber's face. The engine makes one per fiber and keeps it while the fiber lives; its
  // context, parent and injections are kept here too, so a disposed fiber still answers.
  class Fiber {
    constructor(handle) {
      Object.defineProperty(this, kHandle, { value: handle });
    }
    get uid() { return N.fiberUid(this[kHandle]); }
    get state() { return N.fiberState(this[kHandle]); }
    get ctx() { return this[kCtx]; }
    get context() { return this[kCtx]; }
    get parent() { return this[kParent]; }
    get runtime() { return N.fiberRuntime(this[kHandle]); }
    get inject() {
      const inject = N.fiberInject(this[kHandle]);
      Object.defineProperty(this, 'inject', { value: inject, enumerable: true });
      return inject;
    }
    get config() { return N.fiberConfig(this[kHandle]); }
    set config(value) { N.fiberSetConfig(this[kHandle], value); }
    get store() { return N.fiberStore(this[kHandle]); }
    get _store() { return N.fiberChecked(this[kHandle]); }
    get inertia() { return N.fiberInertia(this[kHandle]); }
    get _error() { return N.fiberError(this[kHandle]); }
    set _error(value) { N.fiberSetError(this[kHandle], value); }
    get dispose() {
      // A plugin fiber's is the effect on its parent that disposes it; the root's restarts.
      const dispose = N.fiberDispose(this[kHandle]) ?? (() => this.restart());
      Object.defineProperty(this, 'dispose', { value: dispose, writable: true, configurable: true });
      return dispose;
    }
    get name() { return N.fiberName(this[kHandle]); }
    get _hooks() {
      const hooks = N.fiberUpdateHooks(this[kHandle]);
      return hooks.length ? { 'internal/update': hooks } : Object.create(null);
    }
    get _disposables() { return N.fiberDisposables(this[kHandle]); }
    assertActive() { N.fiberAssertActive(this[kHandle]); }
    effect(execute, label = 'anonymous') {
      return N.fiberEffect(this[kHandle], execute, String(label));
    }
    getEffects() { return N.fiberEffects(this[kHandle]); }
    async await() {
      await N.fiberAwait(this[kHandle]);
      return this;
    }
    async restart() {
      const fiber = this.ctx.fiber;
      return N.fiberRestart(fiber[kHandle]);
    }
    update(config, noSave = false) {
      const fiber = this.ctx.fiber;
      N.fiberUpdate(fiber[kHandle], config, !!noSave);
    }
    [Symbol.for('nodejs.util.inspect.custom')]() {
      return `Fiber <${this.name}>`;
    }
  }

  function makeEffect(handle) {
    const wrapper = () => N.effectDispose(handle);
    Object.defineProperty(wrapper, kHandle, { value: handle });
    Object.defineProperty(wrapper, symbols.effect, {
      get: () => N.effectMeta(handle),
      configurable: true,
    });
    const disposeAsync = () => N.effectDisposeNow(handle);
    wrapper.then = async (onFulfilled, onRejected) => {
      return Promise.resolve(N.effectTask(handle)).then(() => disposeAsync).then(onFulfilled, onRejected);
    };
    return wrapper;
  }

  function makeRuntime(handle, callback, Config) {
    return {
      get name() { return N.runtimeName(handle); },
      callback,
      get fibers() { return N.runtimeFibers(handle); },
      Config,
    };
  }

  // ─── events.ts ─────────────────────────────────────────────────────────────────────────

  function isBailed(value) {
    return value !== null && value !== false && value !== undefined;
  }

  // `_resolve`'s argument reading: an object or function first is `this`.
  function split(args) {
    const thisArg = typeof args[0] === 'object' || typeof args[0] === 'function' ? args.shift() : null;
    const name = args.shift();
    return [thisArg, name, args];
  }

  class EventsService {
    constructor(ctx) {
      this.ctx = ctx;
      defineProperty(this, symbols.tracker, {
        property: 'ctx',
        noShadow: true,
      });
    }
    ctx;
    get _hooks() { return N.hooks(this.ctx[kRoot]); }
    /** @deprecated */
    dispatch(type, args) {
      // Shifts `this` and the name off `args` itself, as upstream's `_resolve` does: callers
      // (DSH among them) spread what is left into each callback.
      const [thisArg, name, rest] = split(args);
      const callbacks = N.dispatch(this.ctx[kRoot], type, thisArg, name, rest);
      return callbacks.map((callback) => callback.bind(thisArg));
    }
    async parallel(...args) {
      const [thisArg, name, rest] = split(args);
      return N.parallel(this.ctx[kRoot], thisArg, name, rest);
    }
    emit(...args) {
      const [thisArg, name, rest] = split(args);
      N.emit(this.ctx[kRoot], thisArg, name, rest);
    }
    async serial(...args) {
      const [thisArg, name, rest] = split(args);
      return N.serial(this.ctx[kRoot], thisArg, name, rest);
    }
    bail(...args) {
      const [thisArg, name, rest] = split(args);
      return N.bail(this.ctx[kRoot], thisArg, name, rest);
    }
    waterfall(...args) {
      const [thisArg, name, rest] = split(args);
      const inner = rest.pop();
      return N.waterfall(this.ctx[kRoot], thisArg, name, rest, inner);
    }
    unregister(hooks, callback) {
      const index = hooks.findIndex((hook) => hook.callback === callback);
      if (index >= 0) {
        hooks.splice(index, 1);
        return true;
      }
    }
    on(name, listener, options) {
      if (typeof options !== 'object') {
        options = { prepend: options };
      }
      this.ctx.fiber.assertActive();
      listener = this.ctx.reflect.bind(listener);
      const label = `ctx.on(${typeof name === 'string' ? JSON.stringify(name) : name.toString()})`;
      return N.on(this.ctx[kRoot], this.ctx, name, listener, options ?? {}, label);
    }
    once(name, listener, options) {
      const dispose = this.on(name, function (...args) {
        dispose();
        return listener.apply(this, args);
      }, options);
      return dispose;
    }
  }

  // ─── logger.ts ─────────────────────────────────────────────────────────────────────────

  const LoggerLevel = ((LoggerLevel2) => {
    LoggerLevel2[LoggerLevel2['ERROR'] = 0] = 'ERROR';
    LoggerLevel2[LoggerLevel2['WARN'] = 1] = 'WARN';
    LoggerLevel2[LoggerLevel2['INFO'] = 2] = 'INFO';
    LoggerLevel2[LoggerLevel2['DEBUG'] = 3] = 'DEBUG';
    return LoggerLevel2;
  })({});

  const defaultFormatters = {
    s: (value) => String(value),
    d: (value) => Math.trunc(Number(value)),
    i: (value) => Math.trunc(Number(value)),
    f: (value) => Number(value),
    o: (value) => JSON.stringify(value),
    O: (value) => JSON.stringify(value),
    c: () => '',
    C: (value, exporter, message) => {
      return Logger.color(exporter, Logger.code(message.name, exporter.colors), value);
    },
  };

  function isAggregateError(error) {
    return error instanceof Error && Array.isArray(error['errors']);
  }

  class Logger {
    constructor(options, service) {
      this.service = service;
      Object.assign(this, options);
      this.error = this._method('error', 0);
      this.info = this._method('info', 2);
      this.warn = this._method('warn', 1);
      this.debug = this._method('debug', 3);
    }
    service;
    static color(exporter, code, value, decoration = '') {
      if (!exporter.colors) return '' + value;
      return `\x1B[3${code < 8 ? code : '8;5;' + code}${exporter.colors >= 2 ? decoration : ''}m${value}\x1B[0m`;
    }
    static code(name, level) {
      let hash = 0;
      for (let i = 0; i < name.length; i++) {
        hash = (hash << 3) - hash + name.charCodeAt(i) + 13;
        hash |= 0;
      }
      const colors = !level ? [] : level >= 2 ? c256 : c16;
      return colors[Math.abs(hash) % colors.length];
    }
    static format(exporter, message) {
      const args = message.args.slice();
      if (args[0] instanceof Error) {
        args[0] = args[0].stack || args[0].message;
        args.unshift('%s');
      } else if (typeof args[0] !== 'string') {
        args.unshift('%o');
      }
      let format = args.shift();
      format = format.replace(/%([a-zA-Z%])/g, (match, char) => {
        if (match === '%%') return '%';
        const formatter = exporter.formatters?.[char] ?? defaultFormatters[char];
        if (typeof formatter === 'function') {
          const value = args.shift();
          return formatter(value, exporter, message);
        }
        return match;
      });
      const oFormatter = exporter.formatters?.o ?? defaultFormatters.o;
      for (let arg of args) {
        if (typeof arg === 'object' && arg) {
          arg = oFormatter(arg, exporter, message);
        }
        format += ' ' + arg;
      }
      const { maxLength = 10240 } = exporter;
      return format.split(/\r?\n/g).map((line) => {
        return line.slice(0, maxLength) + (line.length > maxLength ? '...' : '');
      }).join('\n');
    }
    _method(type, level) {
      return (...args) => {
        if (args.length === 1 && args[0] instanceof Error) {
          if (args[0].cause) {
            this[type](args[0].cause);
          } else if (isAggregateError(args[0])) {
            args[0].errors.forEach((error) => this[type](error));
            return;
          }
        }
        const sn = ++this.service._snMessage;
        const ts = Date.now();
        for (const exporter of this.service.exporters.values()) {
          const targetLevel = exporter.levels?.[this.name] ?? exporter.levels?.default ?? this.level ?? 2;
          if (targetLevel < level) continue;
          const message = { sn, ts, type, level, name: this.name, ...this.meta, args };
          exporter.export(message);
        }
      };
    }
  }

  const c16 = [6, 2, 3, 4, 5, 1];
  const c256 = [
    20, 21, 26, 27, 32, 33, 38, 39, 40, 41, 42, 43, 44, 45, 56, 57, 62, 63, 68, 69, 74, 75, 76,
    77, 78, 79, 80, 81, 92, 93, 98, 99, 112, 113, 129, 134, 135, 148, 149, 160, 161, 162, 163,
    164, 165, 166, 167, 168, 169, 170, 171, 172, 173, 178, 179, 184, 185, 196, 197, 198, 199,
    200, 201, 202, 203, 204, 205, 206, 207, 208, 209, 214, 215, 220, 221,
  ];

  class LoggerService {
    bufferSize = 1e3;
    buffer = [];
    ctx;
    _snMessage = 0;
    _snExporter = 0;
    exporters = new Map();
    constructor(ctx) {
      const tracker = {
        property: 'ctx',
        noShadow: true,
      };
      const self = createCallable('logger', joinPrototype(Object.getPrototypeOf(this), Function.prototype), tracker);
      Object.assign(self, this);
      self.ctx = ctx;
      defineProperty(self, symbols.tracker, tracker);
      self.exporter({
        colors: 3,
        export: (message) => {
          self.buffer.push(message);
          const overflow = self.buffer.length - self.bufferSize;
          if (overflow === 1) {
            self.buffer.shift();
          } else if (overflow > 1) {
            self.buffer.splice(0, overflow);
          }
        },
      });
      return self;
    }
    exporter(exporter) {
      return this.ctx.effect(() => {
        const id = ++this._snExporter;
        this.exporters.set(id, exporter);
        return () => this.exporters.delete(id);
      }, 'ctx.logger.exporter()');
    }
    _resolveConfig() {
      let intercept = this.ctx[symbols.intercept];
      const configs = [];
      while ('logger' in intercept) {
        if (Object.hasOwn(intercept, 'logger')) {
          configs.unshift(intercept['logger']);
        }
        intercept = Object.getPrototypeOf(intercept);
      }
      return Object.assign({}, ...configs);
    }
    [symbols.invoke](name) {
      const config = this._resolveConfig();
      const caller = this[symbols.caller];
      const fiber = (caller ?? this.ctx).fiber;
      name ??= config.name;
      name ??= hyphenate(fiber.name);
      return new Logger({
        name,
        level: config.level,
        meta: { fiber: new WeakRef(fiber) },
      }, this);
    }
    static {
      for (const type of ['error', 'info', 'warn', 'debug']) {
        LoggerService.prototype[type] = function (...args) {
          return this()[type](...args);
        };
      }
    }
  }

  // ─── reflect.ts ────────────────────────────────────────────────────────────────────────

  function enhanceError(error) {
    if (typeof error.stack === 'string') {
      const lines = error.stack.split('\n');
      lines.splice(0, 2, `Error: ${error.message}`);
      error.stack = lines.join('\n');
    }
    return error;
  }

  const RESERVED_WORDS = ['prototype', 'then'];

  function isSpecialProperty(prop) {
    return typeof prop === 'symbol' || RESERVED_WORDS.includes(prop) || parseInt(prop).toString() === prop || prop.startsWith('_');
  }

  class ReflectService {
    constructor(ctx) {
      this.ctx = ctx;
      defineProperty(this, symbols.tracker, {
        property: 'ctx',
        noShadow: true,
      });
      this.mixin('reflect', ['get', 'set', 'provide', 'accessor', 'mixin']);
      this.mixin('fiber', ['runtime', 'effect']);
      this.mixin('registry', ['inject', 'plugin']);
      this.mixin('events', ['on', 'once', 'parallel', 'emit', 'serial', 'bail', 'waterfall']);
    }
    ctx;
    static handler = {
      get: (target, prop, ctx) => {
        if (isSpecialProperty(prop)) {
          return Reflect.get(target, prop, ctx);
        }
        if (Reflect.has(target, prop)) {
          return getTraceable(ctx, Reflect.get(target, prop, ctx));
        }
        const error = new Error(`cannot get property "${prop}" without inject`);
        try {
          const def = N.prop(target[kRoot], prop);
          if (def?.type === 'accessor') {
            return def.get.call(ctx, ctx[symbols.receiver], error);
          }
          if (!ctx.fiber.runtime) return ctx.reflect.get(prop, false);
          return ctx.events.waterfall('internal/get', ctx, prop, error, () => {
            const found = N.lookup(target[kRoot], (ctx[symbols.shadow] ?? ctx).fiber, prop);
            if (found.length) return getTraceable(ctx, found[0]);
            if (found.inactive) {
              error.message = `cannot get required service "${prop}" in inactive context`;
            }
            throw error;
          });
        } catch (e) {
          throw e === error ? enhanceError(e) : e;
        }
      },
      set: (target, prop, value, ctx) => {
        if (isSpecialProperty(prop)) {
          return Reflect.set(target, prop, value, ctx);
        }
        const error = new Error(`cannot set property "${prop}" without provide`);
        const def = N.prop(target[kRoot], prop);
        if (!def) {
          if (!ctx.fiber.runtime) return Reflect.set(target, prop, value, ctx);
          throw enhanceError(error);
        }
        try {
          if (def.type === 'accessor') {
            if (!def.set) return false;
            return def.set.call(ctx, value, ctx[symbols.receiver], error);
          }
          return ctx.events.waterfall('internal/set', ctx, prop, value, error, () => {
            return ctx.reflect.set(prop, value, error);
          });
        } catch (e) {
          throw e === error ? enhanceError(e) : e;
        }
      },
      has: (target, prop) => {
        if (isSpecialProperty(prop)) {
          return Reflect.has(target, prop);
        }
        if (Reflect.has(target, prop)) return true;
        return N.hasProp(target[kRoot], prop);
      },
    };
    get store() { return N.store(this.ctx[kRoot]); }
    get props() {
      const root = this.ctx[kRoot];
      return new Proxy(Object.create(null), {
        get: (_, prop) => (typeof prop === 'string' ? N.prop(root, prop) : undefined),
        has: (_, prop) => typeof prop === 'string' && N.hasProp(root, prop),
        ownKeys: () => N.propNames(root),
        getOwnPropertyDescriptor: (_, prop) => {
          if (typeof prop !== 'string' || !N.hasProp(root, prop)) return undefined;
          return { value: N.prop(root, prop), writable: true, enumerable: true, configurable: true };
        },
      });
    }
    get(name, strict = true) {
      return getTraceable(this.ctx, N.getService(this.ctx[kRoot], this.ctx, name, strict));
    }
    _getImpl(name, strict = true) {
      return N.getImpl(this.ctx[kRoot], this.ctx, name, strict);
    }
    set(name, value, error) {
      N.setService(this.ctx[kRoot], this.ctx, name, value);
      return true;
    }
    provide(name, value, check) {
      return N.provide(this.ctx[kRoot], this.ctx, name, value, check);
    }
    notify(names) {
      return N.notify(this.ctx[kRoot], this.ctx, names);
    }
    accessor(name, options) {
      return N.accessor(this.ctx[kRoot], this.ctx, name, options?.get, options?.set);
    }
    mixin(source, mixins) {
      const self = this;
      return this.ctx.fiber.effect(function* () {
        const entries = Array.isArray(mixins) ? mixins.map((key) => [key, key]) : Object.entries(mixins);
        const getTarget = (ctx, error) => {
          return ctx[source];
        };
        for (const [key, value] of entries) {
          yield self.accessor(value, {
            get(receiver, error) {
              const service = getTarget(this, error);
              if (isNullable(service)) return service;
              const mixin = receiver ? withProps(receiver, service) : service;
              const value2 = Reflect.get(service, key, mixin);
              if (typeof value2 !== 'function') return value2;
              return value2.bind(mixin ?? service);
            },
            set(value2, receiver, error) {
              const service = getTarget(this, error);
              const mixin = receiver ? withProps(receiver, service) : service;
              return Reflect.set(service, key, value2, mixin);
            },
          });
        }
      }, `ctx.mixin(${JSON.stringify(source)})`);
    }
    trace(value) {
      return getTraceable(this.ctx, value);
    }
    bind(callback) {
      return new Proxy(callback, {
        apply: (target, thisArg, args) => {
          return Reflect.apply(target, this.trace(thisArg), args.map((arg) => this.trace(arg)));
        },
        construct: (target, args, newTarget) => {
          return Reflect.construct(target, args.map((arg) => this.trace(arg)), newTarget);
        },
      });
    }
  }

  // ─── registry.ts ───────────────────────────────────────────────────────────────────────

  function isApplicable(object) {
    return object && typeof object === 'object' && typeof object.apply === 'function';
  }

  function Inject(name, config) {
    return function (value, decorator) {
      if (decorator.kind === 'class') {
        if (!Object.hasOwn(value, 'inject')) {
          defineProperty(value, 'inject', Object.create(Object.getPrototypeOf(value).inject ?? null));
          defineProperty(value.inject, symbols.checkProto, true);
        }
        value.inject[name] = config;
      } else if (decorator.kind === 'method') {
        const inject = (value[symbols.metadata] ??= {}).inject ??= Object.create(null);
        inject[name] = config;
        decorator.addInitializer(function () {
          const property = this[symbols.tracker]?.property;
          (this[symbols.initHooks] ??= []).push(() => {
            this.ctx.inject(inject, (ctx) => {
              return value.call(property ? withProps(this, { [property]: ctx }) : this);
            });
          });
        });
      } else {
        throw new Error('@Inject() can only be used on class or class methods');
      }
    };
  }
  Inject.resolve = function resolve(inject, result = Object.create(null)) {
    if (!inject) return result;
    if (Array.isArray(inject)) {
      for (const name of inject) {
        result[name] = null;
      }
    } else if (Reflect.has(inject, symbols.checkProto)) {
      Object.assign(result, resolve(Object.getPrototypeOf(inject)));
      for (const name of Object.keys(inject)) {
        result[name] = inject[name] ?? null;
      }
    } else {
      for (const name of Object.keys(inject)) {
        result[name] = inject[name] ?? null;
      }
    }
    return result;
  };

  class RegistryService {
    constructor(ctx) {
      this.ctx = ctx;
      defineProperty(this, symbols.tracker, {
        property: 'ctx',
        noShadow: true,
      });
    }
    ctx;
    get counter() { return N.counter(this.ctx[kRoot]); }
    get size() { return N.runtimeCount(this.ctx[kRoot]); }
    resolve(plugin) {
      try {
        if (typeof plugin === 'function') return plugin;
        if (isApplicable(plugin)) return plugin.apply;
      } catch {}
    }
    get(plugin) {
      const key = this.resolve(plugin);
      return key && N.runtimeGet(this.ctx[kRoot], key);
    }
    has(plugin) {
      const key = this.resolve(plugin);
      return !!key && N.runtimeGet(this.ctx[kRoot], key) !== undefined;
    }
    delete(plugin) {
      const key = this.resolve(plugin);
      if (!key) return;
      return N.runtimeDelete(this.ctx[kRoot], key);
    }
    keys() { return N.runtimes(this.ctx[kRoot]).map(([key]) => key).values(); }
    values() { return N.runtimes(this.ctx[kRoot]).map(([, runtime]) => runtime).values(); }
    entries() { return N.runtimes(this.ctx[kRoot]).values(); }
    forEach(callback) {
      for (const [key, runtime] of N.runtimes(this.ctx[kRoot])) callback(runtime, key, this);
    }
    inject(inject, callback) {
      return this.plugin({ inject, apply: callback, name: callback.name });
    }
    plugin(plugin, config, getOuterStack) {
      const callback = this.resolve(plugin);
      if (!callback) throw new Error('invalid plugin, expect function or object with an "apply" method, received ' + typeof plugin);
      this.ctx.fiber.assertActive();
      let name = plugin.name;
      if (name === 'apply') name = undefined;
      const inject = Object.entries(Inject.resolve(plugin.inject));
      const fiber = N.plugin(this.ctx[kRoot], this.ctx, callback, typeof name === 'string' ? name : undefined, plugin.Config, inject, config);
      const wrapped = Object.create(fiber);
      wrapped.then = (onFulfilled, onRejected) => {
        return fiber.await().then(onFulfilled, onRejected);
      };
      return wrapped;
    }
  }

  // ─── context.ts ────────────────────────────────────────────────────────────────────────

  class Context {
    static effect = symbols.effect;
    static filter = symbols.filter;
    static isolate = symbols.isolate;
    static intercept = symbols.intercept;
    static is(value) {
      return !!value?.[Context.is];
    }
    static {
      Context.is[Symbol.toPrimitive] = () => Symbol.for('cordis.is');
      Context.prototype[Context.is] = true;
    }
    constructor() {
      this[symbols.isolate] = Object.create(null);
      this[symbols.intercept] = Object.create(null);
      const self = new Proxy(this, ReflectService.handler);
      this.root = self;
      this.baseUrl = undefined;
      const root = N.createRoot(self, helpers);
      Object.defineProperty(this, kRoot, { value: root });
      this.fiber = N.rootFiber(root);
      this.reflect = new ReflectService(self);
      this.registry = new RegistryService(self);
      this.events = new EventsService(self);
      this.logger = new LoggerService(self);
      N.forgetRootDisposables(root);
      return self;
    }
    [Symbol.for('nodejs.util.inspect.custom')]() {
      return `Context <${this.fiber.name}>`;
    }
    extend(meta = {}) {
      const shadow = Reflect.getOwnPropertyDescriptor(this, symbols.shadow)?.value;
      const self = Object.create(getTraceable(this, this));
      for (const prop of Reflect.ownKeys(meta)) {
        Object.defineProperty(self, prop, Reflect.getOwnPropertyDescriptor(meta, prop));
      }
      if (!shadow) return self;
      return Object.assign(Object.create(self), { [symbols.shadow]: shadow });
    }
    isolate(name, label) {
      const shadow = Object.create(this[symbols.isolate]);
      shadow[name] = label ?? Symbol(name);
      return this.extend({ [symbols.isolate]: shadow });
    }
    intercept(name, config) {
      const intercept = Object.create(this[symbols.intercept]);
      intercept[name] = config;
      return this.extend({ [symbols.intercept]: intercept });
    }
  }

  // ─── service.ts ────────────────────────────────────────────────────────────────────────

  class Service {
    constructor(ctx, name) {
      this.ctx = ctx;
      name ??= this.constructor['provide'];
      let self = this;
      const tracker = {
        associate: name,
        property: 'ctx',
      };
      if (self[symbols.invoke]) {
        self = createCallable(name, joinPrototype(Object.getPrototypeOf(this), Function.prototype), tracker);
      }
      self.ctx = ctx;
      self.name = name;
      defineProperty(self, symbols.tracker, tracker);
      self.ctx.reflect.provide(name, self, this[symbols.check]);
      return self;
    }
    ctx;
    static init = symbols.init;
    static check = symbols.check;
    static config = symbols.config;
    static invoke = symbols.invoke;
    static extend = symbols.extend;
    static tracker = symbols.tracker;
    static resolveConfig = symbols.resolveConfig;
    name;
    [symbols.filter](ctx) {
      return ctx[symbols.isolate][this.name] === this.ctx[symbols.isolate][this.name];
    }
    [symbols.extend](props) {
      let self;
      if (this[Service.invoke]) {
        self = createCallable(this.name, this, this[symbols.tracker]);
      } else {
        self = Object.create(this);
      }
      return Object.assign(self, props);
    }
    [symbols.resolveConfig](base, head) {
      let intercept = this.ctx[Context.intercept];
      const configs = [];
      while (this.name in intercept) {
        if (Object.hasOwn(intercept, this.name)) {
          configs.unshift(intercept[this.name]);
        }
        intercept = Object.getPrototypeOf(intercept);
      }
      if (base) configs.unshift(base);
      if (head) configs.push(head);
      if (this['Config']?.merge) {
        return this['Config'].merge(...configs);
      } else {
        return Object.assign({}, ...configs);
      }
    }
    static [Symbol.hasInstance](instance) {
      if (!instance) return false;
      let constructor = instance.constructor;
      while (constructor) {
        constructor = constructor.prototype?.constructor;
        if (constructor === this) return true;
        constructor &&= Object.getPrototypeOf(constructor);
      }
      return false;
    }
  }

  // ─── What the engine asks of this side ─────────────────────────────────────────────────

  const helpers = {
    makeFiber: (handle) => new Fiber(handle),
    makeEffect,
    makeRuntime,
    deriveFiberCtx(parent, fiber, entries, hasInject) {
      const ctx = parent.extend({ fiber });
      if (hasInject) {
        ctx[symbols.intercept] = Object.create(parent[symbols.intercept]);
        for (const [name, config] of entries) {
          ctx[symbols.intercept][name] = config;
        }
      }
      Object.defineProperty(fiber, kCtx, { value: ctx, configurable: true });
      Object.defineProperty(fiber, kParent, { value: parent, configurable: true });
      return ctx;
    },
    rootFiberCtx(fiber, ctx) {
      Object.defineProperty(fiber, kCtx, { value: ctx, configurable: true });
      Object.defineProperty(fiber, kParent, { value: ctx, configurable: true });
    },
    executePlugin(runtime, callback, ctx, config) {
      if (isConstructor(callback)) {
        const instance = new callback(ctx, config);
        for (const hook of instance?.[symbols.initHooks] ?? []) {
          hook();
        }
        return instance?.[symbols.init]?.();
      }
      return Reflect.apply(callback, runtime, [ctx, config]);
    },
    resolveConfig: (Config, config) => resolveConfig({ Config }, config),
    checkService: (check, ctx, value) => !!check.call(getTraceable(ctx, value)),
    report(ctx, error) {
      ctx.logger.error(error);
    },
    makeError(kind, message) {
      if (kind === 'TypeError') return new TypeError(message);
      if (kind === 'Error') return new Error(message);
      return new CordisError(kind, message);
    },
    composeError(reason) {
      if (typeof reason?.stack !== 'string') return new Error(reason);
      return reason;
    },
    aggregate: (errors) => new AggregateError(errors),
    iterate(value) {
      if (Symbol.iterator in value) return [1, value[Symbol.iterator]()];
      if (Symbol.asyncIterator in value) return [2, value[Symbol.asyncIterator]()];
      return [0];
    },
  };

  return {
    Context,
    CordisError,
    DisposableList,
    EventsService,
    Fiber,
    FiberState,
    Inject,
    Logger,
    LoggerLevel,
    LoggerService,
    RegistryService,
    ReflectService,
    Service,
    ValidationError,
    buildOuterStack,
    c16,
    c256,
    composeError,
    createCallable,
    defaultFormatters,
    getPropertyDescriptor,
    getTraceable,
    isBailed,
    isConstructor,
    isObject,
    joinPrototype,
    resolveConfig,
    symbols,
    withProps,
  };
})
