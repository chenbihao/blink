import assert from 'node:assert/strict';
import {dataUrl, loadSource, documentFrame, ScrollTrackingEngine, createRealWorker, encodeTestPng, createScrollThumbnail} from './scroll-test-support.mjs';
import {mkdtemp, writeFile, rm} from 'node:fs/promises';
import {tmpdir} from 'node:os';
import {join, dirname, basename, resolve} from 'node:path';
import {replayExportedDirectory} from './replay-runner.mjs';
const {ScrollTrackingClient} = await import(dataUrl(await loadSource('./tracking-client.js')));

let client = new ScrollTrackingClient({}, createRealWorker);
const recorded = [];
try {
    let engine = new ScrollTrackingEngine();
    const sequence = [0, 18, 36, 18, 0, 18, 900, 900, 18, 18];
    const images = new Map();
    for (const [index, top] of sequence.entries()) {
        const discontinuity = index === 6, trackingReset = index === 8;
        if (trackingReset) {
            const seed = {frames: engine.state.frames, currentTop: engine.state.currentTop,
                trackingState: engine.state.trackingState, height: 90};
            client.dispose(); client = new ScrollTrackingClient(seed, createRealWorker);
            engine = new ScrollTrackingEngine(seed);
            images.clear();
        }
        const frame = documentFrame(top);
        const png = encodeTestPng(frame);
        const expected = engine.process(documentFrame(top), {discontinuity});
        const actual = await client.process(frame, {discontinuity});
        recorded.push({png, discontinuity, trackingReset, expectedDecision: actual.decision});
        assert.equal(frame.data.byteLength, 0, '输入帧通过 transfer 转交 Worker');
        assert.deepEqual(actual.decision, expected.decision, '真实 Worker 与离线状态机必须逐帧一致');
        if (actual.segments) {
            for (const item of actual.segments) {
                if (item.buffer) {
                    images.set(item.id, item.buffer);
                    assert.ok(item.preview, '新增确认片段必须携带 Worker 生成的缩略图');
                    assert.equal(item.preview.buffer.byteLength, item.preview.width * item.preview.height * 4);
                    const expectedPreview = createScrollThumbnail(new ImageData(new Uint8ClampedArray(item.buffer), item.width, item.height));
                    assert.deepEqual(new Uint8ClampedArray(item.preview.buffer), expectedPreview.data);
                } else {
                    assert.ok(images.has(item.id), '历史片段不得重复传像素');
                    assert.equal(item.preview, undefined, '历史缩略图也不能重复生成或传输');
                }
            }
        }
    }
    const composed = await client.compose();
    assert.deepEqual(composed.data, engine.compose().data, 'Worker 最终合成必须像素一致');
    assert.equal(composed.height, engine.compose().height);
} finally { client.dispose(); }

// 宽屏帧的缩略资源有界；窄图、单行片段与透明像素不会丢失或污染边缘。
const wide = new ImageData(1920, 900);
for (let i = 0; i < wide.data.length; i += 4) wide.data.set([30, 90, 150, 255], i);
const small = createScrollThumbnail(wide);
assert.equal(small.width, 120); assert.equal(small.height, 57);
assert.ok(small.data.byteLength < wide.data.byteLength / 250);
for (let i = 0; i < small.data.length; i += 4) assert.deepEqual([...small.data.subarray(i, i + 4)], [30, 90, 150, 255]);
assert.equal(createScrollThumbnail(new ImageData(1920, 1)).height, 1);
const narrow = documentFrame(0, 36, 3);
assert.deepEqual(createScrollThumbnail(narrow).data, narrow.data);
const transparent = new ImageData(240, 1);
for (let i = 0; i < transparent.data.length; i += 8) transparent.data.set([255, 0, 0, 0, 0, 0, 100, 255], i);
assert.deepEqual([...createScrollThumbnail(transparent).data.subarray(0, 4)], [0, 0, 100, 128]);

// 实际 PNG + manifest 走用户使用的目录 runner，不能只在内存里比较算法。
const directory = await mkdtemp(join(tmpdir(), 'blink-scroll-r1-'));
try {
    const frames = [];
    for (const [index, item] of recorded.entries()) {
        const file = `frame-${String(index).padStart(4, '0')}.png`;
        await writeFile(join(directory, file), item.png);
        frames.push({file, discontinuity: item.discontinuity, trackingReset: item.trackingReset,
            expectedDecision: item.expectedDecision});
    }
    await writeFile(join(directory, 'manifest.json'), JSON.stringify({format: 'blink-scroll-replay', version: 1, frames}));
    assert.equal((await replayExportedDirectory(directory)).mismatches.length, 0);
} finally {
    assert.equal(dirname(resolve(directory)), resolve(tmpdir()));
    assert.ok(basename(directory).startsWith('blink-scroll-r1-'));
    await rm(directory, {recursive: true});
}

// 超时/异常重建从已确认片段恢复，而不是把新尾帧误当成位置零的首帧。
const seedEngine = new ScrollTrackingEngine();
for (const top of [0, 18, 36]) seedEngine.process(documentFrame(top));
const restored = new ScrollTrackingClient({frames: seedEngine.state.frames, currentTop: 36, height: 90}, createRealWorker);
try {
    const result = await restored.process(documentFrame(54), {});
    assert.equal(result.currentTop, 54);
    assert.equal((await restored.compose()).height, 144);
} finally { restored.dispose(); }

// 关闭时不等待永远不返回的 IPC，所有等待者立即失败。
const stalled = new ScrollTrackingClient({}, () => ({postMessage() {}, terminate() {}}));
const pending = stalled.process(documentFrame(0), {});
stalled.dispose();
await assert.rejects(pending, /关闭/);
console.log('scroll tracking worker tests passed');
