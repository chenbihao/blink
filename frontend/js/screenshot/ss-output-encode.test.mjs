import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';

// 保留生产模块的编码函数；其他导入只供无关输出动作使用，本测试不触发这些动作。
const source = (await readFile(new URL('./ss-output.js', import.meta.url), 'utf8'))
    .replace(/^import[\s\S]*?from ['"][^'"]+['"];\r?$/gm, '');
const {encodeImageDataPng} = await import('data:text/javascript;base64,' + Buffer.from(source).toString('base64'));
const canvases = [];
globalThis.document = {createElement() {
    const canvas = {width: 0, height: 0,
        getContext: () => ({putImageData(image) { canvas.image = image; }}),
        toBlob(done) { canvas.complete = done; },
    };
    canvases.push(canvas);
    return canvas;
}};
assert.equal(await encodeImageDataPng(null), null);
const firstImage = {width: 1244, height: 3130}, secondImage = {width: 1920, height: 900};
const first = encodeImageDataPng(firstImage), second = encodeImageDataPng(secondImage);
assert.notEqual(canvases[0], canvases[1], '旧会话迟到编码不能读写新会话的画布');
assert.equal(canvases[0].image, firstImage); assert.equal(canvases[1].image, secondImage);
canvases[1].complete(new Blob([new Uint8Array([2, 3])]));
assert.deepEqual(await second, new Uint8Array([2, 3]));
assert.equal(canvases[1].width * canvases[1].height, 0, '编码完成立即释放画布像素');
assert.equal(canvases[0].width, 1244, '新任务完成不能提前释放旧任务仍在编码的画布');
canvases[0].complete(new Blob([new Uint8Array([1])]));
assert.deepEqual(await first, new Uint8Array([1]));
assert.equal(canvases[0].width * canvases[0].height, 0);

const empty = encodeImageDataPng(firstImage);
canvases.at(-1).complete(null);
assert.equal(await empty, null);
assert.equal(canvases.at(-1).width * canvases.at(-1).height, 0, '编码无结果同样释放像素');
document.createElement = () => {
    const canvas = {width: 0, height: 0, getContext() { throw new Error('canvas unavailable'); }};
    canvases.push(canvas); return canvas;
};
await assert.rejects(encodeImageDataPng(firstImage), /canvas unavailable/);
assert.equal(canvases.at(-1).width * canvases.at(-1).height, 0, '异常路径同样释放像素');
console.log('screenshot PNG encoding resource tests passed');

// 编辑后的长图同样使用任务自有画布，不能在编码等待中污染另一会话或长期缓存巨图。
const state = globalThis.longCompositeTest = {
    ss: {selCss: {x: 0, y: 0, w: 2, h: 3}, editorSession: {source: 'long'}, sent: true},
    annot: {hasAnnotations: () => true}, IMAGE_SOURCE: {LONG_SCREENSHOT: 'long'},
};
globalThis.window = {__blinkScreenMeta: {}};
const {compositeSelectionBytes} = await import('data:text/javascript;base64,' + Buffer.from(
    'const {ss, annot, IMAGE_SOURCE} = globalThis.longCompositeTest;\n' + source).toString('base64'));
document.createElement = () => {
    const canvas = {width: 0, height: 0, pixels: new Uint8Array(24),
        getContext: () => ({drawImage(image) {
            for (let i = 0; i < image.pixels.length; i += 4) {
                if (image.pixels[i + 3]) canvas.pixels.set(image.pixels.slice(i, i + 4), i);
            }
        }}),
        toBlob(done) { canvas.finish = done; canvas.complete = () => done(new Blob([canvas.pixels])); },
    };
    canvases.push(canvas); return canvas;
};
const base = {width: 2, height: 3, pixels: new Uint8Array(Array.from({length: 6}, () => [10, 20, 30, 255]).flat())};
const tail = {width: 2, height: 3, pixels: new Uint8Array(24)};
tail.pixels.set([255, 0, 0, 255], 20);
state.ss.editorSession.baseCanvas = base; state.ss.annotCanvas = tail;
const composing = compositeSelectionBytes(), own = canvases.at(-1);
state.ss.editorSession.baseCanvas = {...base, pixels: new Uint8Array(24).fill(255)};
const nextComposing = compositeSelectionBytes(), nextOwn = canvases.at(-1);
assert.notEqual(own, nextOwn);
nextOwn.complete(); await nextComposing;
assert.equal(nextOwn.width * nextOwn.height, 0);
assert.equal(own.width, 2, '另一任务完成不能释放仍在编码的画布');
own.complete();
const expected = base.pixels.slice(); expected.set([255, 0, 0, 255], 20);
assert.deepEqual(await composing, expected, '只编辑末尾时仍完整保留底图和末尾标注');
assert.equal(own.width * own.height, 0);
const failed = compositeSelectionBytes(), failedCanvas = canvases.at(-1);
failedCanvas.finish(null);
assert.equal(await failed, null);
assert.equal(failedCanvas.width * failedCanvas.height, 0, '编码失败同样释放整图画布');
console.log('long screenshot edited PNG snapshot tests passed');
