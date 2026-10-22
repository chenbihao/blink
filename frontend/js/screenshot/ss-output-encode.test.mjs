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
