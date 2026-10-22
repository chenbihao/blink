import assert from 'node:assert/strict';
import {readFile} from 'node:fs/promises';
const dataUrl = source => 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
const sourceOf = name => readFile(new URL(name, import.meta.url), 'utf8');
const classes = () => {
    const values = new Set(['hidden']);
    return {add: key => values.add(key), remove: key => values.delete(key), contains: key => values.has(key),
        toggle: (key, enabled) => enabled ? values.add(key) : values.delete(key)};
};
let createdCanvases = 0;
const draws = [], strokes = [], dashes = [], uploaded = [], cachedCanvases = [];
const context = {
    clearRect() {}, putImageData: image => uploaded.push(image), save() {}, restore() {}, fillRect() {},
    drawImage: (...args) => draws.push(args.slice(1)),
    strokeRect: (...args) => strokes.push(args),
    setLineDash: pattern => dashes.push(pattern),
};
function element(width, height) {
    return {style: {}, dataset: {}, classList: classes(),
        get offsetWidth() { return parseFloat(this.style.width) || width; },
        offsetHeight: height,
        getBoundingClientRect() {
            const scale = Number(this.style.transform?.match(/scale\(([^)]+)\)/)?.[1] || 1);
            return {left: parseFloat(this.style.left) || 0, top: parseFloat(this.style.top) || 0,
                width: this.offsetWidth * scale, height: height * scale};
        },
    };
}
const toolbar = element(320, 36), diagnostics = element(420, 32);
const nodes = new Map([['scroll-toolbar', toolbar], ['scroll-diagnostics', diagnostics]]);
const canvas = {width: 120, height: 200, style: {}, classList: classes()};
let image = {width: 800, height: 300, data: new Uint8ClampedArray(4)};
const state = globalThis.previewTestState = {
    scrollPreviewCtx: context, scrollPreviewCanvas: canvas, scrollFrames: [{image, top: 0}],
    scrollCurrentTop: 0, scrollBandH: 300, scrollTrackingState: 'tracking', scrollDirection: 'vertical',
    scrollSourceRect: {x: 100, y: 500, w: 500, h: 180},
    scrollSession: {scrollSourceRect: {x: 100, y: 500, w: 500, h: 180}},
};
globalThis.previewScale = 1;
globalThis.previewMonitor = {x: 0, y: 0, w: 1000, h: 700};
globalThis.ImageData = class {
    constructor(width, height) { this.width = width; this.height = height; this.data = new Uint8ClampedArray(width * height * 4); }
};
globalThis.document = {documentElement: {}, getElementById: id => nodes.get(id) || null,
    createElement: () => { createdCanvases++; const canvas = {width: 0, height: 0, getContext: () => context};
        cachedCanvases.push(canvas); return canvas; }};
globalThis.window = {innerWidth: 5000, innerHeight: 2000, __blinkScreenMeta: {}};
globalThis.getComputedStyle = () => ({getPropertyValue: () => '#4f8cff'});
globalThis.requestAnimationFrame = () => assert.fail('输入反馈不能另起一次预测重绘');

const urls = {
    '../ss-state.js': dataUrl('export const ss = globalThis.previewTestState;'),
    './stitch.js': dataUrl(`export const positionedFrameBounds = frames => ({
        top: Math.min(...frames.map(f => f.top)), bottom: Math.max(...frames.map(f => f.top + f.image.height))});`),
    './thumbnail.js': dataUrl(await sourceOf('./thumbnail.js')),
    './preview-layout.js': dataUrl(await sourceOf('./preview-layout.js')),
    '../ss-selection-geometry.js': dataUrl('export const uiScaleAtCss = () => globalThis.previewScale;'),
    '../ss-display.js': dataUrl('export const findDisplayCssAt = () => globalThis.previewMonitor; export const getMonitorForScroll = () => globalThis.previewMonitor;'),
    '../ss-utils.js': dataUrl(await sourceOf('../ss-utils.js')),
};
let source = await sourceOf('./preview.js');
for (const [name, url] of Object.entries(urls)) source = source.replace(`'${name}'`, JSON.stringify(url));
const {showScrollInputFeedback, updatePreview, resetPreviewRendering,
    positionScrollToolbar, positionScrollDiagnostics} = await import(dataUrl(source));

updatePreview(); updatePreview();
assert.equal(createdCanvases, 1, '同一片段的 Canvas 缓存应复用');
assert.equal(draws.length, 1, '确认图像与定位框均未变化时应跳过重复重绘');
assert.equal(cachedCanvases[0].width, 120, '预览缓存不得保留原分辨率 Canvas');
assert.equal(cachedCanvases[0].height, 45);
const initial = {height: canvas.height, top: canvas.style.top,
    draw: draws.at(-1), locator: strokes.at(-1), draws: draws.length};
for (let i = 0; i < 20; i++) showScrollInputFeedback(i % 2 ? -1 : 1);
assert.equal(canvas.classList.contains('is-pending'), true);
assert.equal(draws.length, initial.draws, '滚轮不能用预测位移重绘实际图片');
assert.equal(canvas.height, initial.height);
assert.equal(canvas.style.top, initial.top);
updatePreview({pendingInput: true});
assert.deepEqual(draws.at(-1), initial.draw);
assert.deepEqual(strokes.at(-1), initial.locator);
updatePreview();
assert.equal(canvas.classList.contains('is-pending'), false);

// 远距离候选不能扩充确认范围、缩放画布或移动内容窗口。
updatePreview({candidateTop: -3000});
assert.equal(canvas.height, initial.height);
assert.deepEqual(draws.at(-1), initial.draw);
state.scrollTrackingState = 'lost';
updatePreview({candidateTop: 120});
assert.deepEqual(dashes.at(-1), [4,3]);
assert.equal(context.strokeStyle, '#4f8cff');
updatePreview(); assert.equal(context.strokeStyle, '#f59e0b');
state.scrollTrackingState = 'tracking';

// 新增片段只上传自己的 Worker 缩略图；历史片段不再上传或缩放原图。
const thumbnail = new ImageData(120, 15);
const second = {image: {...image, height: 100}, preview: thumbnail, top: 300};
state.scrollFrames = [...state.scrollFrames, second];
const uploadsBefore = uploaded.length;
updatePreview();
assert.equal(uploaded.length, uploadsBefore + 1);
assert.equal(uploaded.at(-1), thumbnail, '直接使用 Worker 提供的小图');
assert.equal(cachedCanvases.at(-1).width, 120);
assert.equal(cachedCanvases.at(-1).height, 15);
const settledDraws = draws.length;
state.scrollFrames = state.scrollFrames.map(frame => ({...frame}));
updatePreview();
assert.equal(draws.length, settledDraws, '仅包装对象更新不应重绘历史像素');
state.scrollCurrentTop = 50; updatePreview();
assert.ok(draws.length > settledDraws, '反向或回访时定位变化必须及时呈现');
assert.equal(uploaded.length, uploadsBefore + 1, '回访只重用缩略缓存');
state.scrollCurrentTop = 0;

// 实际长图逐步增高时外壳位置保持不变，不再为每次增长向上移动。
image = {...image, height: 1200}; state.scrollFrames = [{image, top: 0}];
updatePreview(); assert.equal(canvas.style.top, initial.top);
assert.equal(canvas.height, 180);

// 来源副屏矮于虚拟桌面、且有 DPI 补偿：预览和诊断条都必须留在该屏。
globalThis.previewMonitor = {x: 1920, y: 0, w: 1280, h: 400};
globalThis.previewScale = 1.5;
state.scrollSourceRect = state.scrollSession.scrollSourceRect = {x: 2300, y: 250, w: 300, h: 90};
state.scrollFrames = [{image: {...image, height: 8000}, top: 0}];
updatePreview();
assert.equal(canvas.height, 256);
assert.ok(parseFloat(canvas.style.top) + canvas.height * 1.5 <= 392);
diagnostics.classList.remove('hidden'); positionScrollToolbar(state.scrollSourceRect);
const tb = toolbar.getBoundingClientRect(), panel = diagnostics.getBoundingClientRect();
assert.ok(panel.left >= 1928 && panel.left + panel.width <= 3192);
assert.ok(panel.top >= 8 && panel.top + panel.height <= 392);
assert.ok(panel.top + panel.height <= tb.top - 8 || panel.top >= tb.top + tb.height + 8,
    '诊断条应在工具栏附近避让，不能遮挡按钮');

// 负坐标来源屏同样不能被错误钳制到全局 0。
globalThis.previewMonitor = {x: -1920, y: -1080, w: 1920, h: 1080};
state.scrollSourceRect = state.scrollSession.scrollSourceRect = {x: -1200, y: -500, w: 500, h: 300};
positionScrollToolbar(state.scrollSourceRect); positionScrollDiagnostics();
const negative = diagnostics.getBoundingClientRect();
assert.ok(negative.left >= -1912 && negative.left + negative.width <= -8);
assert.ok(negative.top >= -1072 && negative.top + negative.height <= -8);
const cachedBeforeReset = createdCanvases;
resetPreviewRendering(); assert.equal(canvas.classList.contains('is-pending'), false);
updatePreview();
assert.equal(createdCanvases, cachedBeforeReset + 1, '会话重置后不得复用旧会话的呈现状态');
console.log('scroll preview tests passed');
