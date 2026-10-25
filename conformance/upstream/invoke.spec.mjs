// Ported from cordis@4.0.0-rc.8 packages/core/tests/invoke.spec.ts (MIT, cordiverse/cordis).
// Types stripped by tools/port-specs.cjs; behaviour unchanged.
import { expect, describe, it } from 'vitest';
import { Context, Service } from 'cordis';
describe('Functional Service', ()=>{
    it('functional service', async ()=>{
        class Foo extends Service {
            config;
            constructor(ctx, config){
                super(ctx, 'foo'), this.config = config;
            }
            [Service.invoke](init) {
                expect(this.ctx).to.be.instanceof(Context);
                let result = {
                    ...this.config
                };
                let intercept = this.ctx[Context.intercept];
                while(intercept){
                    Object.assign(result, intercept.foo);
                    intercept = Object.getPrototypeOf(intercept);
                }
                Object.assign(result, init);
                return result;
            }
            invoke() {
                return this();
            }
            extend(config) {
                return this[Service.extend]({
                    config: {
                        ...this.config,
                        ...config
                    }
                });
            }
        }
        const root = new Context();
        await root.plugin(Foo, {
            a: 1
        });
        expect(root.foo()).to.deep.equal({
            a: 1
        });
        const ctx1 = root.intercept('foo', {
            b: 2
        });
        expect(ctx1.foo()).to.deep.equal({
            a: 1,
            b: 2
        });
        const foo1 = ctx1.foo;
        expect(foo1).to.be.instanceof(Foo);
        const foo2 = root.foo.extend({
            c: 3
        });
        expect(foo2).to.be.instanceof(Foo);
        expect(foo2()).to.deep.equal({
            a: 1,
            c: 3
        });
        const foo3 = foo1.extend({
            d: 4
        });
        expect(foo3).to.be.instanceof(Foo);
        expect(foo3.invoke()).to.deep.equal({
            a: 1,
            b: 2,
            d: 4
        });
        expect(foo1.invoke()).to.deep.equal({
            a: 1,
            b: 2
        });
    });
    it('uses the service shadow for callable extensions', async ()=>{
        class Dependency extends Service {
            constructor(ctx){
                super(ctx, 'dependency');
            }
        }
        class Callable extends Service {
            static inject = [
                'dependency'
            ];
            constructor(ctx){
                super(ctx, 'callable');
            }
            [Service.invoke]() {
                return this.ctx['dependency'];
            }
            extend() {
                return this[Service.extend]();
            }
        }
        class Outer extends Service {
            static inject = [
                'callable'
            ];
            constructor(ctx){
                super(ctx, 'outer');
            }
            call() {
                const callable = this.ctx['callable'];
                return [
                    callable(),
                    callable.extend()()
                ];
            }
        }
        const root = new Context();
        await root.plugin(Dependency);
        await root.plugin(Callable);
        await root.plugin(Outer);
        let result;
        await root.inject([
            'outer'
        ], (ctx)=>{
            result = ctx['outer'].call();
        });
        expect(result[0]).toBeInstanceOf(Dependency);
        expect(result[1]).toBeInstanceOf(Dependency);
    });
});
