//! 预热 OCR 静默激活划词测试（0.24.x）。
//!
//! 覆盖 `activateSilentOcrReading`：
//! 1. 正常路径：挂 ocrResultCache + overlay(mode=null) + 进入 reading
//! 2. ss.ocrBusy（用户已点识别/翻译在等预热）→ 跳过，交给 activateOverlay
//! 3. ss.reading 已激活 → 不重建，保留已划选区
//! 4. 无有效文字行 → 完全静默
//! 5. 有 lines 无 words → cache/overlay 挂上，reading 不进入（enterReadingMode 内部保护）
//! 6. overlay mode=null 不产生可见嵌字（mode 字段断言）

import {describe, test, beforeEach} from 'node:test';
import assert from 'node:assert';

// Mock window before importing production modules (api.js → tauri.js)
globalThis.window = globalThis;
globalThis.document = {
    getElementById: () => null,
    createElement: () => ({
        style: {},
        setAttribute() {
        },
        removeAttribute() {
        },
        addEventListener() {
        },
        appendChild() {
        },
    }),
    body: {
        appendChild() {
        },
        contains: () => false,
        classList: {add() {
        }, remove() {
        }},
    },
    addEventListener() {
    },
    removeEventListener() {
    },
    querySelector: () => null,
    querySelectorAll: () => [],
};

function makeFakeHitCanvas() {
    const listeners = {};
    return {
        style: {},
        width: 0,
        height: 0,
        listeners,
        setAttribute() {
        },
        removeAttribute() {
        },
        addEventListener(ev, fn) {
            (listeners[ev] = listeners[ev] || []).push(fn);
        },
        removeEventListener() {
        },
        setPointerCapture() {
        },
        releasePointerCapture() {
        },
        hasPointerCapture: () => false,
        getContext: () => ({
            clearRect() {
            },
            fillRect() {
            },
            strokeRect() {
            },
        }),
    };
}

const {ss} = await import('./ss-state.js');
const annot = await import('./annotation-engine.js');
const {activateSilentOcrReading} = await import('./ss-ocr.js');

const VALID_RESULT = {
    text: '你好世界\nsecond line',
    lines: [
        {text: '你好世界', rect: {x: 10, y: 10, w: 80, h: 20}},
        {text: 'second line', rect: {x: 10, y: 40, w: 100, h: 20}},
    ],
    words: [
        {text: '你好世界', rect: {x: 10, y: 10, w: 80, h: 20}, line_index: 0},
        {text: 'second line', rect: {x: 10, y: 40, w: 100, h: 20}, line_index: 1},
    ],
};

beforeEach(() => {
    ss.selCss = {x: 100, y: 100, w: 300, h: 200};
    ss.ocrBusy = false;
    ss.ocrResultCache = null;
    ss.reading = null;
    ss.hitEventsBound = false;
    ss.hitCanvas = makeFakeHitCanvas();
    ss.hitCtx = ss.hitCanvas.getContext();
    ss.editorSession.beginScreenshotSelection();
    annot.clearOverlay();
});

describe('activateSilentOcrReading — 预热 OCR 静默激活划词', () => {
    test('正常路径：挂 cache + overlay(mode=null) + 进入 reading', () => {
        activateSilentOcrReading(VALID_RESULT);

        assert.strictEqual(ss.ocrResultCache, VALID_RESULT, '应缓存预热结果供点[识别]秒开');
        const overlay = annot.getOverlay();
        assert.ok(overlay, '应挂 overlay 行数据');
        assert.strictEqual(overlay.mode, null, 'mode=null 图上不画任何可见文字');
        assert.strictEqual(overlay.lines.length, 2);
        assert.strictEqual(overlay.lines[0].srcText, '你好世界');
        assert.ok(ss.reading, '应进入划词模式');
        assert.strictEqual(ss.reading.words.length, 2);
    });

    test('mode=null 时后续点[翻译]可复用行数据（dstText 初始为空）', () => {
        activateSilentOcrReading(VALID_RESULT);
        const overlay = annot.getOverlay();
        assert.ok(overlay.lines.every((l) => l.dstText === null), '行数据应保留 dstText=null 供翻译回填');
    });

    test('ss.ocrBusy=true（用户已点识别/翻译）→ 跳过静默激活', () => {
        ss.ocrBusy = true;
        activateSilentOcrReading(VALID_RESULT);

        assert.strictEqual(ss.ocrResultCache, null, '不应抢在 activateOverlay 之前挂 cache');
        assert.strictEqual(annot.getOverlay(), null, '不应挂 overlay');
        assert.strictEqual(ss.reading, null, '不应进入划词');
    });

    test('ss.reading 已激活 → 不重建，保留已划选区', () => {
        activateSilentOcrReading(VALID_RESULT);
        const firstReading = ss.reading;
        firstReading.selectionStart = 0;
        firstReading.selectionEnd = 1;

        activateSilentOcrReading(VALID_RESULT);

        assert.strictEqual(ss.reading, firstReading, 'reading 不应被重建');
        assert.strictEqual(ss.reading.selectionStart, 0, '已划选区应保留');
    });

    test('无有效文字行 → 完全静默', () => {
        activateSilentOcrReading({text: '', lines: [], words: []});

        assert.strictEqual(ss.ocrResultCache, null);
        assert.strictEqual(annot.getOverlay(), null);
        assert.strictEqual(ss.reading, null);
    });

    test('rect 无效的行被过滤', () => {
        activateSilentOcrReading({
            text: 'a',
            lines: [
                {text: 'a', rect: {x: 0, y: 0, w: 10, h: 10}},
                {text: 'bad', rect: {x: 0, y: 0, w: 0, h: 10}},
            ],
            words: [{text: 'a', rect: {x: 0, y: 0, w: 10, h: 10}, line_index: 0}],
        });
        const overlay = annot.getOverlay();
        assert.strictEqual(overlay.lines.length, 1, 'w=0 的行不应进入 overlay');
    });

    test('有 lines 无 words → cache/overlay 挂上，reading 不进入', () => {
        activateSilentOcrReading({
            text: 'hello',
            lines: [{text: 'hello', rect: {x: 5, y: 5, w: 50, h: 12}}],
            words: [],
        });

        assert.strictEqual(ss.ocrResultCache?.text, 'hello', 'cache 应挂上（点识别秒开）');
        assert.ok(annot.getOverlay(), 'overlay 行数据应挂上');
        assert.strictEqual(ss.reading, null, '无 words 时划词不可用（enterReadingMode 内部保护）');
    });

    test('canvas 编辑器（长图/剪贴板/钉图）→ 跳过静默激活（左键拖拽=平移，不可被划词拦截）', () => {
        ss.editorSession.beginCanvasSource('long-screenshot', {width: 100, height: 100});

        activateSilentOcrReading(VALID_RESULT);

        assert.strictEqual(ss.ocrResultCache, null, 'canvas 编辑器不挂 cache/不进划词，维持纯预热');
        assert.strictEqual(annot.getOverlay(), null);
        assert.strictEqual(ss.reading, null);
    });
});
