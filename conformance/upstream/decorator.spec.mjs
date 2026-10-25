// Ported from cordis@4.0.0-rc.8 packages/core/tests/decorator.spec.ts (MIT, cordiverse/cordis).
// Types stripped by tools/port-specs.cjs; behaviour unchanged.
import { Context, Service } from 'cordis';
import { Inject } from 'cordis';
import { expect, describe, it } from 'vitest';
import { mock } from 'node:test';
describe('Decorator', ()=>{
    it('@Inject on class method', async ()=>{
        const callback = mock.fn();
        const dispose = mock.fn();
        class Foo extends Service {
            constructor(ctx){
                super(ctx, 'foo');
            }
        }
        let Bar = class Bar extends Service {
            constructor(ctx){
                super(ctx, 'bar');
            }
            method() {
                callback();
                return dispose;
            }
        };
        {
            const initializers = [];
            const decorated = Inject('foo')(Bar.prototype.method, {
                kind: 'method',
                name: 'method',
                addInitializer: (fn)=>initializers.push(fn)
            });
            if (decorated) Bar.prototype.method = decorated;
            const Original = Bar;
            Bar = class extends Original {
                constructor(...args){
                    super(...args);
                    for (const fn of initializers)fn.call(this);
                }
            };
        }
        const root = new Context();
        await root.plugin(Bar);
        expect(callback.mock.calls).to.have.length(0);
        expect(dispose.mock.calls).to.have.length(0);
        const fiber = await root.plugin(Foo);
        expect(callback.mock.calls).to.have.length(1);
        expect(dispose.mock.calls).to.have.length(0);
        await fiber.dispose();
        expect(callback.mock.calls).to.have.length(1);
        expect(dispose.mock.calls).to.have.length(1);
    });
});
