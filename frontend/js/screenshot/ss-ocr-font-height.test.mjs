//! OCR 行字号参考高度（fontH）透传测试。
//!
//! 覆盖 0.23 引入的 PP-OCR det 框 unclip 折减链路：
//! 后端 `OcrLine.font_height`（JSON `font_h`）→ ss-ocr overlay 行 `fontH`
//! → annotation-engine overlayLayer 保留。
//!
//! 语义：
//! 1. 后端下发 font_h → overlay 行携带 fontH（字号推导用，rect 不变）
//! 2. WinRT 等未下发 font_h 的结果 → fontH=null（渲染回退 rect.h）
//! 3. 非法 font_h（0/负数/非数字）→ fontH=null

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

/** 构造 OCR 结果：每行 rect.h=40，font_h 可按行覆盖。 */
function makeResult(fontHs) {
    return {
        text: 'line1\nline2',
        lines: [
            {
                text: 'line1',
                rect: {x: 10, y: 10, w: 80, h: 40},
                ...(fontHs[0] === undefined ? {} : {font_h: fontHs[0]}),
            },
            {
                text: 'line2',
                rect: {x: 10, y: 60, w: 100, h: 40},
                ...(fontHs[1] === undefined ? {} : {font_h: fontHs[1]}),
            },
        ],
        words: [
            {text: 'line1', rect: {x: 10, y: 10, w: 80, h: 40}, line_index: 0},
            {text: 'line2', rect: {x: 10, y: 60, w: 100, h: 40}, line_index: 1},
        ],
    };
}

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

describe('OCR 行 fontH 透传（PP-OCR det unclip 折减）', () => {
    test('后端下发 font_h → overlay 行携带 fontH，rect 原样保留', () => {
        activateSilentOcrReading(makeResult([28, 28]));

        const overlay = annot.getOverlay();
        assert.strictEqual(overlay.lines.length, 2);
        for (const line of overlay.lines) {
            assert.strictEqual(line.fontH, 28, 'fontH 应取后端 font_h');
            assert.strictEqual(line.rect.h, 40, 'rect 本身不折减（背景覆盖用）');
        }
    });

    test('WinRT 结果（无 font_h）→ fontH=null，渲染回退 rect.h', () => {
        activateSilentOcrReading(makeResult([]));

        const overlay = annot.getOverlay();
        for (const line of overlay.lines) {
            assert.strictEqual(line.fontH, null);
        }
    });

    test('非法 font_h（0 / 负数 / 非数字）→ fontH=null', () => {
        activateSilentOcrReading(makeResult([0, -5]));
        let overlay = annot.getOverlay();
        for (const line of overlay.lines) {
            assert.strictEqual(line.fontH, null, '0 和负数都视为未下发');
        }

        annot.clearOverlay();
        const bad = makeResult([28]);
        bad.lines[0].font_h = 'not-a-number';
        // 首次激活已进入划词模式，重新激活前重置（模拟新会话）
        ss.reading = null;
        activateSilentOcrReading(bad);
        overlay = annot.getOverlay();
        assert.strictEqual(overlay.lines[0].fontH, null, '非数字视为未下发');
    });
});
