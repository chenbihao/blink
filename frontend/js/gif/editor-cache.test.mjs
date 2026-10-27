import test from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

// 执行编辑器真实的解码/缓存段，DOM 渲染与原生 IPC 使用边界桩。
const source = readFileSync(new URL('./editor.js', import.meta.url), 'utf8')
    .split('// ── 时间轴（')[0].replace(/^import .*;\r?$/gm, '');

function fixture(invoke, createImageBitmap) {
    const context = vm.createContext({
        document: {getElementById() { return {}; }},
        ensureSpriteLoaded() {}, refreshLiveThumbs() {},
        invoke, createImageBitmap,
        fetch: async () => ({blob: async () => ({})}), console,
    });
    vm.runInContext(`${source}\nglobalThis.fixture = {state, pumpDecode};`, context);
    const {state, pumpDecode} = context.fixture;
    state.sessionId = 'a';
    state.view = {frameCount: 100};
    return {state, pumpDecode};
}

test('preview preloading releases bitmaps as each batch reaches the memory budget', async () => {
    let liveBytes = 0;
    let peakBytes = 0;
    let closed = 0;
    const frameBytes = 1280 * 720 * 4;
    const {state, pumpDecode} = fixture(
        async (_command, {indices}) => indices.map(index => ({index, pngBase64: '', width: 1280, height: 720})),
        async () => {
            liveBytes += frameBytes;
            peakBytes = Math.max(peakBytes, liveBytes);
            return {width: 1280, height: 720, close() { closed++; liveBytes -= frameBytes; }};
        },
    );
    for (let start = 0; start < 100; start += 24) {
        state.pendingBatches.push({sessionId: 'a', epoch: 0, maxW: 1280, cache: state.previewCache,
            indices: Array.from({length: Math.min(24, 100 - start)}, (_, i) => i + start)});
    }
    await pumpDecode();
    assert.ok(closed > 0);
    assert.ok(state.previewBytes <= 96 << 20);
    assert.equal(state.previewBytes, liveBytes);
    assert.ok(peakBytes <= (96 << 20) + frameBytes);
});

test('a decode response from an old session cannot populate the new session cache', async () => {
    let resolve;
    let decoded = 0;
    const {state, pumpDecode} = fixture(() => new Promise(r => { resolve = r; }), async () => { decoded++; });
    state.pendingBatches.push({sessionId: 'a', epoch: 0, maxW: 1280, cache: state.previewCache, indices: [0]});
    const task = pumpDecode();
    state.sessionId = 'b';
    state.epoch++;
    resolve([{index: 0, pngBase64: '', width: 1280, height: 720}]);
    await task;
    assert.equal(decoded, 0);
    assert.equal(state.previewCache.size, 0);
    assert.equal(state.previewBytes, 0);
});
