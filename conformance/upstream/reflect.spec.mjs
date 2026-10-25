// Ported from cordis@4.0.0-rc.8 packages/core/tests/reflect.spec.ts (MIT, cordiverse/cordis).
// Types stripped by tools/port-specs.cjs; behaviour unchanged.
import { expect, describe, it } from 'vitest';
import { mock } from 'node:test';
import { Context } from 'cordis';
describe('Reflect', ()=>{
    it('Context.is()', ()=>{
        class SubContext extends Context {
        }
        expect(Context.is(new SubContext())).to.equal(true);
    });
    it('access check', async ()=>{
        const root = new Context();
        await root.plugin((ctx)=>{
            expect(()=>ctx['prototype']).to.not.throw();
            expect(()=>ctx.constructor).to.not.throw();
            expect(()=>ctx.bar).to.throw('cannot get property "bar" without inject');
            expect(()=>ctx.bar = 0).to.throw('cannot set property "bar" without provide');
        });
        await root.plugin((ctx)=>{
            expect(()=>ctx.foo = 0).to.throw('cannot set property "foo" without provide');
            expect(()=>ctx.provide('foo')).to.not.throw();
            expect(()=>ctx.provide('foo')).to.throw('service "foo" has been registered at <root>');
            expect(()=>ctx.foo = 0).to.not.throw();
        });
    });
    it('service injection', async ()=>{
        const root = new Context();
        const warn = mock.fn();
        root.logger.warn = warn;
        root.mixin('foo', [
            'bar'
        ]);
        root.provide('foo');
        root.set('foo', {
            bar: 1
        });
        expect(root.get('foo')).to.be.ok;
        expect(root.get('bar')).to.be.undefined;
        expect(root.get('root')).to.be.undefined;
        root.inject([
            'foo'
        ], (ctx)=>{
            warn.mock.resetCalls();
            expect(warn.mock.calls).to.have.length(0);
            ctx.extend({
                baz: 2
            }).plugin((ctx)=>{
                warn.mock.resetCalls();
                expect(ctx.baz).to.equal(2);
                expect(warn.mock.calls).to.have.length(0);
            });
        });
    });
    it('service inject leak', async ()=>{
        const root = new Context();
        root.provide('foo');
        root.set('foo', {
            bar: 1
        });
        const fiber = await root.inject([
            'foo'
        ], ()=>{});
        expect(fiber.ctx.foo).to.be.ok;
        await fiber.dispose();
        expect(()=>fiber.ctx.foo).to.throw('cannot get required service "foo" in inactive context');
    });
});
