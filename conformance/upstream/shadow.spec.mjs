// Ported from cordis@4.0.0-rc.8 packages/core/tests/shadow.spec.ts (MIT, cordiverse/cordis).
// Types stripped by tools/port-specs.cjs; behaviour unchanged.
import { Context, Service, symbols } from 'cordis';
import { describe, expect, it } from 'vitest';
describe('Traceable caller', ()=>{
    it('keeps caller metadata separate from the service shadow', async ()=>{
        let innerOrigin;
        let outerOrigin;
        class Inner extends Service {
            constructor(ctx){
                super(ctx, 'inner');
                innerOrigin = ctx;
            }
            inspect() {
                return {
                    caller: this[symbols.caller],
                    shadow: this.ctx[symbols.shadow]
                };
            }
        }
        class Outer extends Service {
            static inject = [
                'inner'
            ];
            constructor(ctx){
                super(ctx, 'outer');
                outerOrigin = ctx;
            }
            inspect() {
                const result = this.ctx['inner'].inspect();
                return {
                    ...result,
                    outerShadow: this.ctx[symbols.shadow]
                };
            }
        }
        const root = new Context();
        await root.plugin(Inner);
        await root.plugin(Outer);
        let result;
        await root.inject([
            'outer'
        ], (ctx)=>{
            result = ctx['outer'].inspect();
        });
        expect(result.caller).toBe(outerOrigin);
        expect(result.shadow).toBe(innerOrigin);
        expect(result.outerShadow).toBe(outerOrigin);
    });
    it('exposes the caller without preserving shadow for noShadow services', async ()=>{
        let outerOrigin;
        class Probe {
            ctx;
            [Service.tracker] = {
                property: 'ctx',
                noShadow: true
            };
            constructor(ctx){
                this.ctx = ctx;
            }
            inspect() {
                return {
                    caller: this[symbols.caller],
                    shadow: this.ctx[symbols.shadow]
                };
            }
        }
        class Outer extends Service {
            static inject = [
                'probe'
            ];
            constructor(ctx){
                super(ctx, 'outer');
                outerOrigin = ctx;
            }
            inspect() {
                return this.ctx['probe'].inspect();
            }
        }
        const root = new Context();
        root.provide('probe', new Probe(root));
        await root.plugin(Outer);
        let result;
        await root.inject([
            'outer'
        ], (ctx)=>{
            result = ctx['outer'].inspect();
        });
        expect(result.caller).toBe(outerOrigin);
        expect(result.shadow).toBeUndefined();
    });
    it('exposes the caller to callable services', async ()=>{
        let outerOrigin;
        class Callable extends Service {
            constructor(ctx){
                super(ctx, 'callable');
            }
            [Service.invoke]() {
                return this[symbols.caller];
            }
        }
        class Outer extends Service {
            static inject = [
                'callable'
            ];
            constructor(ctx){
                super(ctx, 'outer');
                outerOrigin = ctx;
            }
            call() {
                return this.ctx['callable']();
            }
        }
        const root = new Context();
        await root.plugin(Callable);
        await root.plugin(Outer);
        let caller;
        await root.inject([
            'outer'
        ], (ctx)=>{
            caller = ctx['outer'].call();
        });
        expect(caller).toBe(outerOrigin);
    });
    it('strips service shadow before creating plugins', async ()=>{
        class Loader extends Service {
            constructor(ctx){
                super(ctx, 'loader');
            }
            load(plugin) {
                return this.ctx.plugin(plugin);
            }
        }
        class Server extends Service {
            constructor(ctx){
                super(ctx, 'server');
            }
        }
        let injected = false;
        function Consumer(ctx) {
            return ctx.inject([
                'server'
            ], ({ server })=>{
                injected = server instanceof Server;
            });
        }
        const root = new Context();
        await root.plugin(Loader);
        await root.inject([
            'loader'
        ], async (ctx)=>{
            const loader = ctx['loader'];
            await loader.load(Server);
            await loader.load(Consumer);
        });
        expect(injected).toBe(true);
        expect(root.logger.buffer.filter((message)=>message.type === 'error')).toHaveLength(0);
    });
});
