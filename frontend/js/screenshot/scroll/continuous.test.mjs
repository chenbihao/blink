import assert from 'node:assert/strict';
import {dataUrl, loadSource} from './scroll-test-support.mjs';
const {ContinuousScrollCapture} = await import(dataUrl(await loadSource('./continuous.js')));
const deferred = () => { let resolve; const promise = new Promise(done => { resolve = done; }); return {promise, resolve}; };
const flush = async () => { for (let i = 0; i < 80; i++) await Promise.resolve(); };
function setup(overrides = {}) {
    let clock = 0, nextId = 0, frames = 0;
    const timers = new Map(), processed = [], captures = [], pressures = [], errors = [];
    const pipeline = new ContinuousScrollCapture({frameBytes: 4, maxBytes: 12, maxFrames: 3,
        now: () => clock,
        setTimer: (fn, ms) => { const id = ++nextId; timers.set(id, {fn, at: clock + ms}); return id; },
        clearTimer: id => timers.delete(id),
        capture: async () => { captures.push(clock); return {frame: ++frames}; },
        process: async (frame, metadata) => { processed.push({frame, metadata}); return {moved: frame < 3}; },
        onPressure: stats => pressures.push(stats), onError: error => errors.push(error),
        ...overrides,
    });
    const advance = async ms => {
        const end = clock + ms;
        await flush();
        for (;;) {
            const next = [...timers].sort((a,b) => a[1].at - b[1].at)[0];
            if (!next || next[1].at > end) break;
            clock = next[1].at; timers.delete(next[0]); next[1].fn(); await flush();
        }
        clock = end; await flush();
    };
    return {pipeline, advance, captures, processed, pressures, errors, timers};
}

// 连续采样与输入事件无关；空闲降频，唤醒不会等待旧的空闲定时器。
{
    const h = setup(); h.pipeline.start(); await h.advance(1000);
    assert.deepEqual(h.captures.slice(0,4), [0,33,66,99]);
    assert.ok(h.captures.slice(1).some((at,i) => at - h.captures[i] === 250));
    const before = h.captures.length; h.pipeline.wake(); await h.advance(0);
    assert.equal(h.captures.length, before + 1);
    await h.pipeline.pauseAndDrain();
    assert.deepEqual(h.pipeline.stats(), {frames: 0, bytes: 0, pressured: false});
    const count = h.captures.length; await h.advance(500);
    assert.equal(h.captures.length, count);
    h.pipeline.dispose(); assert.equal(h.timers.size, 0);
}
// 积压只保留 FIFO 中间帧，不覆盖最新；采集在途、排队和处理中的帧均占预算。
{
    const gate = deferred(), handled = [];
    let active = 0, peak = 0;
    const h = setup({process: async (frame, metadata) => {
        active++; peak = Math.max(peak, active);
        if (frame === 1) await gate.promise;
        handled.push({frame, metadata}); active--;
        return {moved: true};
    }});
    h.pipeline.start(); await h.advance(200);
    assert.equal(h.captures.length, 3);
    assert.deepEqual(h.pipeline.stats(), {frames: 3, bytes: 12, pressured: true});
    assert.equal(h.pressures.length, 1);
    gate.resolve(); await flush();
    assert.deepEqual(handled.map(item => item.frame), [1,2,3]);
    await h.advance(33);
    assert.equal(handled[3].metadata.discontinuity, true, '采样中断后不能假定帧连续');
    assert.equal(peak, 1, '只能有一个处理/提交者');
    h.pipeline.dispose();
}
// 强制收尾先排空旧帧，再采截止帧；重复 sample 有界串行，不重复抓屏。
{
    const gate = deferred(), order = [];
    const h = setup({process: async frame => {
        if (frame === 1) await gate.promise;
        order.push(frame); return {moved: true, frame};
    }});
    h.pipeline.start(); await h.advance(66);
    const sample = h.pipeline.sample({finalizing: true});
    const drain = h.pipeline.pauseAndDrain();
    await flush(); assert.deepEqual(order, []);
    gate.resolve();
    assert.equal((await sample).frame, 4);
    await drain;
    assert.deepEqual(order, [1,2,3,4]);
    assert.equal(h.pipeline.paused, true);
    h.pipeline.dispose();
}
// 旧处理迟到/取消时不再提交；队列资源在退出后释放。
{
    const gate = deferred(), committed = [];
    const h = setup({process: async () => { await gate.promise; return {moved: true}; },
        onResult: result => committed.push(result)});
    h.pipeline.start(); await h.advance(66); h.pipeline.dispose();
    gate.resolve(); await h.pipeline.drain();
    assert.equal(committed.length, 0);
    assert.equal(h.pipeline.stats().bytes, 0);
    assert.equal(h.pipeline.stats().frames, 0);
}
console.log('scroll continuous capture tests passed');
