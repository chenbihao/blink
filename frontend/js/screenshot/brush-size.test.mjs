import assert from 'node:assert/strict';
import {brushDiameter, clampSize, stepSize, createSizeWheel} from './brush-size.js';

globalThis.window = {__TAURI__: {internal: {invoke: async () => {}}},
    __blinkScreenMeta: {renderScaleX: 2, renderScaleY: 2}};
globalThis.document = {getElementById: () => null, documentElement: {classList: {contains: () => false}}};
const {ss} = await import('./ss-state.js');
const annot = await import('./annotation-engine.js');
const {createCanvasSizeWheel, bindSizeControls} = await import('./ss-brush-controls.js');

assert.equal(clampSize('stroke', -5), 1);
assert.equal(clampSize('brush', 1000), 128);
assert.equal(clampSize('brush', NaN), null);
for (const [cat, a, b] of [['stroke', 8, 10], ['brush', 16, 18], ['brush', 48, 52]]) {
    assert.equal(stepSize(cat, a, 1), b);
    assert.equal(stepSize(cat, b, -1), a);
}
assert.equal(stepSize('brush', 128, 1), 128);
assert.equal(stepSize('stroke', 1, -1), 1);
assert.equal(brushDiameter('pixelate', 32), 64);
assert.equal(brushDiameter('eraser', 16), 96);

const accum = createSizeWheel();
const delta = (deltaY, deltaMode = 0) => ({deltaY, deltaMode});
assert.equal(accum.steps(delta(40), 'a'), 0);
assert.equal(accum.steps(delta(60), 'a'), 1);
assert.equal(accum.steps(delta(-60), 'a'), 0);
assert.equal(accum.steps(delta(60), 'a'), 0, '换向清理余量');
assert.equal(accum.steps(delta(40), 'b'), 0, '工具切换不继承余量');
assert.equal(accum.steps(delta(2, 1), 'b'), 1);
assert.equal(accum.steps(delta(1, 2), 'c'), 1);

ss.canvas = {};
ss.strokeCursor = {style: {}};
ss.isAnnotating = true;
ss.selCss = {x: 0, y: 0, w: 100, h: 100};
ss._showTransientHint = () => {};
annot.setTool('pixelate');
annot.setBrushSize(32);
const onWheel = createCanvasSizeWheel();
let consumed = 0;
const event = extra => ({target: ss.canvas, clientX: 50, clientY: 50, deltaY: -100, deltaMode: 0,
    preventDefault() { consumed++; }, stopPropagation() {}, ...extra});
onWheel(event());
assert.equal(annot.getBrushSize(), 34);
assert.equal(ss.strokeCursor.style.width, '34px', '滚轮直接刷新光标，按真实 renderScale 换算');
for (const extra of [{target: {}}, {clientX: 120}, {ctrlKey: true}]) onWheel(event(extra));
assert.equal(annot.getBrushSize(), 34, '面板、选区外、修饰滚轮不调尺寸');
ss.scrollSession.scrollCapturePhase = 'capturing';
onWheel(event());
assert.equal(annot.getBrushSize(), 34, '长截图采集保留滚轮');
ss.scrollSession.scrollCapturePhase = 'editing';
onWheel(event());
assert.equal(annot.getBrushSize(), 36, '长截图编辑允许调尺寸');
ss.scrollSession.scrollCapturePhase = 'idle';
annot.setToolMode('pixelate', 'box');
onWheel(event());
assert.equal(annot.getBrushSize(), 36, '框选模式不抢滚轮');
annot.setToolMode('pixelate', 'brush');
ss.isAnnotDragging = true;
onWheel(event());
assert.equal(annot.getBrushSize(), 36);
ss.isAnnotDragging = false;

// 引擎锁定本笔尺寸；提交后调整配置不改变旧命令。
annot.setTool('pencil');
annot.setStrokeWidth(4);
annot.startDraw(0, 0);
annot.setStrokeWidth(20);
assert.equal(annot.getStrokeWidth(), 4);
annot.moveDraw(10, 10);
annot.endDraw(10, 10);
annot.setStrokeWidth(20);
assert.equal(annot.getCommands()[0].width, 4);

// 记录真实渲染调用，验证数字与模糊的历史尺寸不随全局值变化。
const strokes = [], radii = [];
function fakeCanvas() {
    const canvas = {width: 100, height: 100};
    const ctx = new Proxy({canvas, stroke() { strokes.push(this.lineWidth); },
        arc(x, y, radius) { radii.push(radius); }}, {get(target, key) {
        return key in target ? target[key] : () => {};
    }});
    canvas.getContext = () => ctx;
    return canvas;
}
document.createElement = fakeCanvas;
const canvas = fakeCanvas();
annot.init(canvas);
annot.reset(100, 100, {width: 100, height: 100, data: new Uint8ClampedArray(40000)});
annot.setBrushSize(16);
annot.setTool('number');
annot.startDraw(10, 10);
annot.endDraw(10, 10);
const number = annot.getCommands()[0];
annot.setTool('blur');
annot.startDraw(10, 10);
annot.moveDraw(20, 20);
annot.endDraw(20, 20);
const blur = annot.getCommands()[1];
annot.setBrushSize(48);
radii.length = 0;
strokes.length = 0;
annot.executeCommand(number, canvas.getContext());
annot.executeCommand(blur, canvas.getContext());
assert.equal(radii[0], 19.2);
assert.ok(strokes.includes(32), '旧模糊笔迹仍按 16×2 渲染，而非新的 48×2');
// 马赛克遮罩和光标在 200% 显示下的真实覆盖范围一致。
annot.setBrushSize(32);
annot.setTool('pixelate');
annot.startDraw(10, 10);
annot.moveDraw(20, 20);
annot.endDraw(20, 20);
const mosaic = annot.getCommands()[2];
annot.setBrushSize(48);
strokes.length = 0;
annot.executeCommand(mosaic, canvas.getContext());
assert.ok(strokes.includes(64), '旧马赛克遮罩宽度固定为 64 位图像素');

// 快捷档、滑杆、按钮滚轮共享状态回填；自定义值不误选某个预设档。
const elements = new Map();
function element(dataset = {}) {
    return {dataset, value: '', textContent: '', events: {}, active: false,
        classList: {toggle(name, active) { this.owner.active = active; }},
        addEventListener(name, cb) { this.events[name] = cb; }};
}
for (const [prefix, field, values] of [['stroke-width', 'strokeWidth', [1, 2, 4, 6, 8]],
    ['brush-size', 'brushSize', [8, 16, 24, 32, 48]]]) {
    const items = values.map(value => {
        const item = element({[field]: String(value)});
        item.classList.owner = item;
        return item;
    });
    const dropdown = element();
    dropdown.querySelectorAll = () => items;
    elements.set(`${prefix}-dropdown`, dropdown);
    for (const suffix of ['range', 'value', 'trigger', 'trigger-icon']) elements.set(`${prefix}-${suffix}`, element());
}
document.getElementById = id => elements.get(id) || null;
bindSizeControls();
const slider = elements.get('brush-size-range');
slider.value = '31';
slider.events.input();
assert.equal(annot.getBrushSize(), 31);
assert.equal(elements.get('brush-size-value').textContent, '31 px');
assert.ok(elements.get('brush-size-dropdown').querySelectorAll().every(item => !item.active));
elements.get('brush-size-trigger').events.wheel(event());
assert.equal(annot.getBrushSize(), 32);
assert.equal(slider.value, 32);
const preset = elements.get('brush-size-dropdown').querySelectorAll()[0];
preset.events.click();
assert.equal(annot.getBrushSize(), 8);
assert.equal(elements.get('brush-size-trigger-icon').textContent, 8);

console.log('brush sizes: wheel boundaries, DPI feedback, locked strokes and stable history passed');
