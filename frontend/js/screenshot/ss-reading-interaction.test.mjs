//! OCR 命中层的真实事件处理序列：双击确认、拖动划词与中断清理。
import {beforeEach, test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {runInNewContext} from 'node:vm';

globalThis.window = globalThis;
window.innerWidth = 1920;
window.innerHeight = 1080;

let panelOpen;
let focusCalls;
let confirmed;
let opened;
const textarea = {
    hidden: false,
    focus() { focusCalls++; },
    setSelectionRange() {},
};
globalThis.document = {
    getElementById: (id) => id === 'ocr-panel' && panelOpen
        ? {querySelector: () => textarea}
        : null,
};

const {ss} = await import('./ss-state.js');
const {IMAGE_SOURCE} = await import('./image-editor-session.js');
const {enterReadingMode, exitReadingMode, clearReadingSelection, getReadingSelectionText} = await import('./ss-reading.js');

function makeCanvas() {
    const listeners = new Map();
    const attributes = new Map();
    const captured = new Set();
    return {
        style: {}, width: 1920, height: 1080,
        setAttribute: (name, value) => attributes.set(name, value),
        removeAttribute: (name) => attributes.delete(name),
        getAttribute: (name) => attributes.get(name),
        addEventListener: (type, fn) => listeners.set(type, fn),
        setPointerCapture: (id) => captured.add(id),
        hasPointerCapture: (id) => captured.has(id),
        releasePointerCapture: (id) => captured.delete(id),
        fire(type, x, y, overrides = {}) {
            const event = {
                offsetX: x, offsetY: y,
                clientX: ss.selCss.x + x, clientY: ss.selCss.y + y,
                pointerId: 1, button: 0,
                preventDefault() {}, stopPropagation() {},
                ...overrides,
            };
            listeners.get(type)?.(event);
            return event;
        },
    };
}

function resultFor({charBoxes = true, scale = 1} = {}) {
    const rect = (x, y, w) => ({x: x * scale, y: y * scale, w: w * scale, h: 10 * scale});
    return {
        text: 'abc\ndef',
        words: [
            {text: 'abc', rect: rect(20, 20, 30), line_index: 0},
            {text: 'def', rect: rect(20, 50, 30), line_index: 1},
        ],
        char_boxes: charBoxes ? [...'abcdef'].map((text, i) => ({
            text, rect: rect(20 + (i % 3) * 10, i < 3 ? 20 : 50, 10),
            line_index: i < 3 ? 0 : 1,
            char_start: i < 3 ? i : i + 1,
            char_end: i < 3 ? i + 1 : i + 2,
        })) : [],
    };
}

function enter(options = {}) {
    const scale = options.scale || 1;
    window.devicePixelRatio = 1.5; // 故意不同于 renderScale
    window.__blinkScreenMeta = {vx: 0, vy: 0, renderScaleX: scale, renderScaleY: scale};
    const result = resultFor(options);
    ss.ocrResultCache = result;
    enterReadingMode(result);
    return result;
}

function click(x, y) {
    ss.hitCanvas.fire('pointerdown', x, y);
    ss.hitCanvas.fire('pointerup', x, y);
}

beforeEach(() => {
    panelOpen = false;
    focusCalls = 0;
    confirmed = [];
    opened = 0;
    ss.selCss = {x: 100, y: 100, w: 300, h: 200};
    ss.editorSession.beginScreenshotSelection();
    ss.screenshot = {};
    ss.isAnnotating = true;
    ss.sent = false;
    ss.singleClickTimeout = null;
    ss.selectionInteraction = null;
    ss.reading = null;
    ss.hitEventsBound = false;
    ss.hitCanvas = makeCanvas();
    ss.hitCtx = {clearRect() {}, fillRect() {}, strokeRect() {}};
    ss.canvas = makeCanvas();
    ss._handleScreenshotDoubleClick = (event) => confirmed.push(event);
    // 旧行为的回归哨兵：双击不能再自动打开 OCR 面板。
    ss._showOcrResult = () => opened++;
    enter();
});

test('按下与微小抖动不产生文字选择，也不抢面板焦点', () => {
    panelOpen = true;
    ss.hitCanvas.fire('pointerdown', 25, 25);
    ss.hitCanvas.fire('pointermove', 27, 26);
    ss.hitCanvas.fire('pointerup', 27, 26);
    assert.equal(getReadingSelectionText(), '');
    assert.equal(focusCalls, 0);
    assert.equal(ss.hitCanvas.hasPointerCapture(1), false);
});

for (const charBoxes of [true, false]) {
    for (const [label, x, y] of [['文字', 25, 25], ['行间窄缝', 25, 33], ['真空白', 200, 150]]) {
        test(`${charBoxes ? '字符框' : 'word 降级'}：${label}双击只确认一次`, () => {
            enter({charBoxes});
            click(x, y);
            click(x, y);
            const event = ss.hitCanvas.fire('dblclick', x, y);
            assert.deepEqual(confirmed, [event]);
            assert.equal(getReadingSelectionText(), '');
            assert.equal(opened, 0);
            assert.equal(focusCalls, 0);
            assert.equal(ss.selectionInteraction, null);
        });
    }
}

test('双击已有文字选择仍确认截图，不扩大为整行', () => {
    ss.reading.selectionStart = 1;
    ss.reading.selectionEnd = 1;
    click(25, 25);
    click(25, 25);
    ss.hitCanvas.fire('dblclick', 25, 25);
    assert.equal(confirmed.length, 1);
    assert.equal(getReadingSelectionText(), 'b');
    assert.equal(opened, 0);
});

for (const scale of [1, 2]) {
    test(`${scale}× renderScale 下拖动达到 3 CSS px 才开始字符选择`, () => {
        enter({scale});
        panelOpen = true;
        ss.hitCanvas.fire('pointerdown', 25, 25);
        ss.hitCanvas.fire('pointermove', 28, 25);
        assert.equal(getReadingSelectionText(), 'a');
        assert.equal(focusCalls, 1);
        ss.hitCanvas.fire('pointerup', 28, 25);
        assert.equal(confirmed.length, 0);
    });
}

test('跨行与反向拖选保留连续文本', () => {
    ss.hitCanvas.fire('pointerdown', 25, 25);
    ss.hitCanvas.fire('pointermove', 35, 55);
    ss.hitCanvas.fire('pointerup', 35, 55);
    assert.equal(getReadingSelectionText(), 'abc\nde');
    ss.hitCanvas.fire('pointerdown', 45, 55);
    ss.hitCanvas.fire('pointermove', 35, 25);
    ss.hitCanvas.fire('pointerup', 35, 25);
    assert.equal(getReadingSelectionText(), 'bc\ndef');
});

test('松手使用最终坐标，不遗漏最后一段文字', () => {
    ss.hitCanvas.fire('pointerdown', 25, 25);
    ss.hitCanvas.fire('pointermove', 35, 25);
    ss.hitCanvas.fire('pointerup', 45, 55);
    assert.equal(getReadingSelectionText(), 'abc\ndef');
});

test('没有字符框时仍能拖动选择跨行 word', () => {
    enter({charBoxes: false});
    click(25, 25);
    assert.equal(getReadingSelectionText(), '');
    ss.hitCanvas.fire('pointerdown', 25, 25);
    ss.hitCanvas.fire('pointermove', 45, 55);
    ss.hitCanvas.fire('pointerup', 45, 55);
    assert.equal(getReadingSelectionText(), 'abc\ndef');
});

for (const type of ['pointercancel', 'lostpointercapture', 'mouseleave']) {
    test(`${type} 清理未激活的划选，迟到移动不会创建选择`, () => {
        ss.hitCanvas.fire('pointerdown', 25, 25);
        ss.hitCanvas.fire(type, 45, 55);
        ss.hitCanvas.fire('pointermove', 45, 55);
        ss.hitCanvas.fire('pointerup', 45, 55);
        assert.equal(getReadingSelectionText(), '');
        assert.equal(ss.reading.dragStart, null);
    });
}

test('清除选择、退出与重新进入阅读模式不残留候选拖动', () => {
    ss.hitCanvas.fire('pointerdown', 25, 25);
    clearReadingSelection();
    ss.hitCanvas.fire('pointermove', 45, 55);
    assert.equal(getReadingSelectionText(), '');
    ss.hitCanvas.fire('pointerdown', 25, 25);
    exitReadingMode();
    enter();
    ss.hitCanvas.fire('pointermove', 45, 55);
    assert.equal(getReadingSelectionText(), '');
});

test('真空白与选区边缘仍分别进入移动、缩放交互', () => {
    ss.hitCanvas.fire('pointerdown', 200, 150);
    assert.equal(ss.selectionInteraction.kind, 'move');
    ss.hitCanvas.fire('pointerup', 200, 150);
    ss.hitCanvas.fire('pointerdown', 0, 50);
    assert.equal(ss.selectionInteraction.kind, 'resize');
    assert.equal(ss.selectionInteraction.handle, 'w');
    ss.hitCanvas.fire('pointerup', 0, 50);
    assert.equal(getReadingSelectionText(), '');
});

test('阅读模式退出后，迟到双击不触发截图确认', () => {
    exitReadingMode();
    ss.hitCanvas.fire('dblclick', 25, 25);
    assert.equal(confirmed.length, 0);
});

test('其他指针的移动、松手和 capture 丢失不结束当前划选', () => {
    ss.hitCanvas.fire('pointerdown', 25, 25);
    for (const type of ['pointermove', 'pointerup', 'lostpointercapture']) {
        ss.hitCanvas.fire(type, 45, 55, {pointerId: 2});
    }
    assert.equal(getReadingSelectionText(), '');
    ss.hitCanvas.fire('pointermove', 45, 55);
    ss.hitCanvas.fire('pointerup', 45, 55);
    assert.equal(getReadingSelectionText(), 'abc\ndef');
});

// index.js 导入即启动整个窗口，直接执行其中的共同入口函数验证编排行为。
const indexSource = readFileSync(new URL('./index.js', import.meta.url), 'utf8');
const confirmSource = indexSource.match(/^function handleScreenshotDoubleClick\(e\) \{[\s\S]*?^}/m)?.[0];
assert.ok(confirmSource, '截图窗口必须提供唯一的双击确认入口');

test('OCR 双击转交共同入口，以主画布 CSS 坐标确认且拒绝重复提交', () => {
    ss.canvas.getBoundingClientRect = () => ({left: 20, top: 30});
    ss.screenshot = {};
    ss.isAnnotating = true;
    ss.sent = false;
    let copyCalls = 0;
    let fullScreenCalls = 0;
    const points = [];
    const clearedTimers = [];
    ss.singleClickTimeout = 7;
    const handler = runInNewContext(`(${confirmSource})`, {
        ss, console,
        clearTimeout: (id) => clearedTimers.push(id),
        pointInEditableImage(point) {
            points.push([point.offsetX, point.offsetY]);
            return true;
        },
        doCopySelection() { copyCalls++; ss.sent = true; },
        doCopyFullScreen() { fullScreenCalls++; },
    });
    ss._handleScreenshotDoubleClick = handler;
    click(25, 25);
    click(25, 25);
    ss.hitCanvas.fire('dblclick', 25, 25);
    ss.hitCanvas.fire('dblclick', 25, 25);
    assert.deepEqual(points, [[105, 95]], '不能误用 OCR 层局部坐标 25,25，也不乘 DPR');
    assert.equal(copyCalls, 1);
    assert.equal(fullScreenCalls, 0);
    assert.deepEqual(clearedTimers, [7]);
    assert.equal(ss.singleClickTimeout, null);
});

test('共同入口保留未就绪保护、全屏复制、选区外不提交和图片编辑来源', () => {
    ss.canvas.getBoundingClientRect = () => ({left: 0, top: 0});
    ss.screenshot = null;
    ss.editorSession.reset();
    ss.sent = false;
    let copyCalls = 0;
    let fullScreenCalls = 0;
    let inside = false;
    const handler = runInNewContext(`(${confirmSource})`, {
        ss, console, clearTimeout,
        pointInEditableImage: () => inside,
        doCopySelection: () => copyCalls++,
        doCopyFullScreen: () => fullScreenCalls++,
    });
    handler({clientX: 125, clientY: 125});
    assert.equal(copyCalls + fullScreenCalls, 0);
    ss.screenshot = {};
    ss.isAnnotating = false;
    handler({clientX: 125, clientY: 125});
    assert.equal(fullScreenCalls, 1);
    ss.isAnnotating = true;
    handler({clientX: 125, clientY: 125});
    assert.equal(copyCalls, 0);
    ss.screenshot = null;
    ss.editorSession.beginCanvasSource(IMAGE_SOURCE.CLIPBOARD, {width: 300, height: 200});
    inside = true;
    handler({clientX: 125, clientY: 125});
    assert.equal(copyCalls, 1);
});
