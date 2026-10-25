// Ported from cordis@4.0.0-rc.8 packages/core/tests/utils.ts (MIT, cordiverse/cordis).
// Types stripped by tools/port-specs.cjs; behaviour unchanged.
import { vi } from 'vitest';
import { Context, Service } from 'cordis';
export function withTimers(fn, config) {
    return async ()=>{
        const ctx = new Context();
        vi.useFakeTimers(config);
        try {
            await fn(ctx);
        } finally{
            vi.useRealTimers();
        }
    };
}
export function sleep(ms = 0) {
    return new Promise((resolve)=>setTimeout(resolve, ms));
}
export const event = 'custom-event';
export class Session {
    flag;
    constructor(flag){
        this.flag = flag;
    }
    [Context.filter](context) {
        if (!context.filter) return true;
        return context.filter(this);
    }
}
export class Filter {
    flag;
    constructor(flag){
        this.flag = flag;
    }
    filter = (session)=>{
        return session.flag === this.flag;
    };
}
export class Counter {
    ctx;
    [Service.tracker] = {
        associate: 'counter',
        property: 'ctx'
    };
    value = 0;
    constructor(ctx){
        this.ctx = ctx;
    }
    increase() {
        return this.ctx.effect(()=>{
            this.value++;
            return ()=>this.value--;
        });
    }
}
export function getHookSnapshot(ctx) {
    const result = {};
    for (const [name, callbacks] of Object.entries(ctx.events._hooks)){
        if (callbacks.length) result[name] = callbacks.length;
    }
    return result;
}
