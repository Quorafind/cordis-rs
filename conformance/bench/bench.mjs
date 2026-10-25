// Rough timings of the hot paths, for comparing the native module with the vendored bundle:
// `cordis-run [--native] conformance/bench/bench.mjs`. Prints `BENCH <name> <ms>` lines.
import test from 'node:test';
import { Context, Service } from 'cordis';

const time = async (name, body) => {
  const start = performance.now();
  await body();
  console.log(`BENCH ${name} ${(performance.now() - start).toFixed(1)}`);
};

test('bench', async () => {
  const root = new Context();
  for (let i = 0; i < 20; i++) root.on('e', () => {});
  await time('emit x20 listeners, 5000 times', () => {
    for (let i = 0; i < 5000; i++) root.emit('e', i);
  });
  await time('load and dispose 500 plugins', async () => {
    for (let i = 0; i < 500; i++) {
      const fiber = root.plugin((ctx) => { ctx.on('x', () => {}); return () => {}; });
      await fiber;
      await fiber.dispose();
    }
  });
  class Counter extends Service {
    value = 0;
    constructor(ctx) { super(ctx, 'counter'); }
    increase() { return ++this.value; }
  }
  await root.plugin(Counter);
  let reader;
  await root.inject(['counter'], (ctx) => { reader = ctx; });
  await time('service access through an injecting context, 20000 times', () => {
    for (let i = 0; i < 20000; i++) reader.counter.increase();
  });
  await time('provide and withdraw a service with 50 injectors, 50 times', async () => {
    for (let i = 0; i < 50; i++) root.inject(['svc'], () => {});
    for (let i = 0; i < 50; i++) {
      const dispose = root.provide('svc', {});
      await new Promise((r) => setTimeout(r, 0));
      await dispose();
    }
  });
});
