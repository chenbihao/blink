import assert from 'node:assert/strict';
import { readFile } from 'node:fs/promises';

const dataUrl = (source) => 'data:text/javascript;base64,' + Buffer.from(source).toString('base64');
const ssUrl = dataUrl('export const ss = globalThis.previewTestState;');
const stitchUrl = dataUrl(`
  export const positionedFrameBounds = (frames) => ({
    top: frames[0].top,
    bottom: frames[0].top + frames[0].image.height,
    height: frames[0].image.height,
  });
`);
const layoutUrl = dataUrl(`
  export const computePreviewPosition = () => ({ left: 0, top: 0 });
  export const computePredictedLocatorTop = (options) => {
    globalThis.predictionCalls.push(options);
    return (Number.isFinite(options.pendingTop) ? options.pendingTop : options.currentTop)
      + options.direction * 100;
  };
`);
// 0.19.16 DPI 适配 mock
const geometryUrl = dataUrl('export function uiScaleAtCss() { return 1; }');
const displayUrl = dataUrl('export function findDisplayCssAt() { return { x: 0, y: 0, w: 1000, h: 700 }; } export function getMonitorForScroll() { return null; }');
const utilsUrl = dataUrl('export function computeFloatingPlacement(opts) { return { left: 0, top: 0 }; }');
let dashedDraws = 0;
globalThis.predictionCalls = [];
const context = {
  clearRect() {}, drawImage() {}, save() {}, restore() {},
  fillRect() {}, strokeRect() {}, setLineDash() { dashedDraws++; }, putImageData() {},
};
let createdCanvases = 0;
globalThis.document = {
  documentElement: {},
  createElement: () => {
    createdCanvases++;
    return { width: 0, height: 0, getContext: () => context };
  },
  getElementById: () => null,
};
globalThis.window = { innerWidth: 1000, innerHeight: 700, __blinkScreenMeta: { vx: 0, vy: 0, renderScaleX: 1, renderScaleY: 1 } };
globalThis.getComputedStyle = () => ({ getPropertyValue: () => '#4f8cff' });
globalThis.requestAnimationFrame = (callback) => { callback(); return 1; };
globalThis.cancelAnimationFrame = () => {};

const image = { width: 800, height: 300, data: new Uint8ClampedArray(4) };
globalThis.previewTestState = {
  scrollPreviewCtx: context,
  scrollPreviewCanvas: {
    width: 120, height: 200, style: {}, classList: { remove() {} },
  },
  scrollFrames: [{ image, top: 0 }],
  scrollCurrentTop: 0,
  scrollBandH: 300,
  scrollTrackingState: 'tracking',
  scrollSourceRect: null,
};
const source = (await readFile(new URL('./preview.js', import.meta.url), 'utf8'))
  .replace("'../ss-state.js'", JSON.stringify(ssUrl))
  .replace("'./stitch.js'", JSON.stringify(stitchUrl))
  .replace("'./preview-layout.js'", JSON.stringify(layoutUrl))
  .replace("'../ss-selection-geometry.js'", JSON.stringify(geometryUrl))
  .replace("'../ss-display.js'", JSON.stringify(displayUrl))
  .replace("'../ss-utils.js'", JSON.stringify(utilsUrl));
const { showPredictedPreview, updatePreview } = await import(dataUrl(source));

updatePreview();
updatePreview();
assert.equal(createdCanvases, 1, '同一 ImageData 的缩略片段 Canvas 必须跨预览更新复用');

showPredictedPreview(1);
const dashedBeforePreserve = dashedDraws;
updatePreview({ preservePrediction: true });
assert.ok(
  dashedDraws > dashedBeforePreserve,
  '旧采集完成时应保留更新一轮滚轮产生的预测定位框',
);

showPredictedPreview(1);
showPredictedPreview(-1);
assert.equal(
  globalThis.predictionCalls.at(-1).pendingTop,
  null,
  '滚轮反向时必须从已确认坐标重新预测，不能沿用反方向累计位置',
);

globalThis.previewTestState.scrollTrackingState = 'lost';
const callsBeforeLostPrediction = globalThis.predictionCalls.length;
showPredictedPreview(1);
assert.equal(
  globalThis.predictionCalls.length,
  callsBeforeLostPrediction,
  'lost 状态不得继续基于陈旧 currentTop 外推定位框',
);
updatePreview({ candidateTop: 120 });
assert.equal(
  context.strokeStyle,
  '#4f8cff',
  '已通过像素复核、等待确认的候选应使用强调色而不是识别失败橙色',
);
updatePreview();
assert.equal(context.strokeStyle, '#f59e0b', '真正 lost 且没有候选时才显示橙色');

console.log('scroll preview tests passed');
