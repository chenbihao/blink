import {describe, test} from 'node:test';
import assert from 'node:assert/strict';
import {fitOverlayLineText, layoutOverlayText, OVERLAY_FONT_HEIGHT_FACTOR} from './overlay-text-layout.js';

// Mock 字形高度等于字号：按产品约定计算参考上限，不锁死校准值。
function expectedSize(h, scale = 1, fontH = h) {
    return Math.floor(Math.min(h, fontH * OVERLAY_FONT_HEIGHT_FACTOR * scale) * 10 + 1e-7) / 10;
}

function assertNear(actual, expected) {
    assert.ok(Math.abs(actual - expected) < 1e-7, `expected ${expected}, got ${actual}`);
}

// 用足够大的参考框保证测试命中“超宽但能缩小完整显示”，而非随调参进入下限省略。
function longLineEntries() {
    const h = Math.max(24, 24 / OVERLAY_FONT_HEIGHT_FACTOR);
    const size = expectedSize(h);
    const shortW = size * 12 + 4;
    const longW = size * 11 * 0.9 / 0.95;
    return [entry('甲'.repeat(10), {h, w: shortW}), entry('乙'.repeat(11), {h, w: longW}), entry('丙'.repeat(10), {h, w: shortW})];
}

function makeContext({heightFactor = 1, overhang = 0, noInkMetrics = false} = {}) {
    const stack = [];
    return {
        font: '12px serif', textAlign: 'right', textBaseline: 'top',
        save() { stack.push([this.font, this.textAlign, this.textBaseline]); },
        restore() { [this.font, this.textAlign, this.textBaseline] = stack.pop(); },
        measureText(text) {
            const size = parseFloat(this.font);
            const width = Array.from(text).length * size;
            return noInkMetrics ? {width} : {
                width,
                actualBoundingBoxLeft: overhang * size,
                actualBoundingBoxRight: width + overhang * size,
                actualBoundingBoxAscent: size * heightFactor * 0.8,
                actualBoundingBoxDescent: size * heightFactor * 0.2,
            };
        },
    };
}

function entry(text = '短句', {w = 200, h = 20, y = 10, fontH = null} = {}) {
    const r = {x: 10, y, w, h};
    return {line: {rect: r, fontH, srcText: text, dstText: text}, r, text};
}

describe('0.25.7 OCR 参考高度与逐行排版', () => {
    test('默认字号采用导出的校准系数，滑杆继续按比例缩放', () => {
        const e = entry();
        for (const scale of [1, 0.6, 1.4]) {
            assert.equal(fitOverlayLineText(makeContext(), e, scale).size, expectedSize(e.r.h, scale));
        }
    });

    test('fontH 优先且不超过框高，非法参考高度回退框高', () => {
        assert.equal(fitOverlayLineText(makeContext(), entry('短句', {fontH: 16})).size, expectedSize(20, 1, 16));
        for (const fontH of [null, 0, -1, NaN, '16', 30]) {
            assert.equal(fitOverlayLineText(makeContext(), entry('短句', {fontH})).size, expectedSize(20));
        }
    });

    test('参考高度相同的长行独立缩小，保留全部译文且不缩小两侧短行', () => {
        const entries = longLineEntries();
        const referenceSize = expectedSize(entries[0].r.h);
        const layouts = layoutOverlayText(makeContext(), entries);
        assert.equal(layouts[0].size, referenceSize);
        assert.equal(layouts[2].size, referenceSize);
        assert.ok(layouts[1].size < referenceSize && layouts[1].size > 8);
        assert.equal(layouts[1].display, entries[1].text);
        assert.equal(layouts[1].truncated, false);
        assert.equal(entries[1].line.dstText, '乙'.repeat(11));
    });

    test('标题与正文高度分组，不被长译文适配后的字号误分层', () => {
        const bodyEntries = longLineEntries().slice(0, 2);
        const title = entry('标题', {h: bodyEntries[0].r.h * 2});
        const layouts = layoutOverlayText(makeContext(), [title, ...bodyEntries]);
        assert.equal(layouts[0].size, expectedSize(title.r.h));
        assert.equal(layouts[1].size, expectedSize(bodyEntries[0].r.h));
        assert.ok(layouts[2].size < layouts[1].size);
        assert.equal(layouts[2].display, bodyEntries[1].text);
    });

    test('实际字形高于字号时继续缩小，实际顶部底部都在原框内', () => {
        const e = entry();
        const ctx = makeContext({heightFactor: 1.4});
        const layout = fitOverlayLineText(ctx, e);
        assert.ok(layout.size < expectedSize(e.r.h));
        ctx.font = `${layout.size}px sans-serif`;
        const m = ctx.measureText(layout.display);
        assert.ok(m.actualBoundingBoxAscent + m.actualBoundingBoxDescent <= Math.min(e.r.h, e.r.h * OVERLAY_FONT_HEIGHT_FACTOR) + 1e-7);
        assert.ok(layout.y - m.actualBoundingBoxAscent >= e.r.y);
        assert.ok(layout.y + m.actualBoundingBoxDescent <= e.r.y + e.r.h);
        assertNear(layout.y + (m.actualBoundingBoxDescent - m.actualBoundingBoxAscent) / 2, e.r.y + e.r.h / 2);
    });

    test('字形偏矮也不反向放大，校准系数始终约束起始字号', () => {
        const e = entry();
        const referenceSize = Math.floor(e.r.h * OVERLAY_FONT_HEIGHT_FACTOR * 10 + 1e-7) / 10;
        assert.equal(fitOverlayLineText(makeContext({heightFactor: 0.5}), e).size, referenceSize);
    });

    test('字形横向外伸计入宽度，绘制坐标补偿左侧外伸', () => {
        const h = Math.max(24, 24 / OVERLAY_FONT_HEIGHT_FACTOR);
        const e = entry('字'.repeat(10), {h, w: expectedSize(h) * 10 / 0.95});
        const ctx = makeContext({overhang: 0.5});
        const layout = fitOverlayLineText(ctx, e);
        assert.ok(layout.size < expectedSize(e.r.h));
        ctx.font = `${layout.size}px sans-serif`;
        const m = ctx.measureText(layout.display);
        assert.ok(layout.x - m.actualBoundingBoxLeft >= e.r.x + 2 - 1e-7);
        assert.ok(layout.x + m.actualBoundingBoxRight <= e.r.x + e.r.w - 2 + 1e-7);
    });

    test('缺少或无效字形指标时回退字号高度，保留上下文状态', () => {
        const ctx = makeContext({noInkMetrics: true});
        const e = entry();
        const layout = fitOverlayLineText(ctx, e);
        assert.equal(layout.size, expectedSize(e.r.h));
        assertNear(layout.y, e.r.y + e.r.h / 2 + layout.size * 0.3);
        assert.equal(ctx.font, '12px serif');
        assert.equal(ctx.textAlign, 'right');
        assert.equal(ctx.textBaseline, 'top');
    });

    test('微小原框不强行撑到 8px', () => {
        const layout = fitOverlayLineText(makeContext(), entry('字', {h: 6, w: 20}));
        assert.equal(layout.size, expectedSize(6));
        assert.equal(layout.display, '字');
    });

    test('仅在字号下限仍超宽时省略，原译文保持完整', () => {
        const e = entry('长'.repeat(30), {w: 100, h: Math.max(24, 24 / OVERLAY_FONT_HEIGHT_FACTOR)});
        const layout = fitOverlayLineText(makeContext(), e);
        assert.equal(layout.size, 8);
        assert.equal(layout.display, '长'.repeat(10) + '…');
        assert.equal(layout.truncated, true);
        assert.equal(e.text, '长'.repeat(30));
    });

    test('省略号也参与高度适配', () => {
        const ctx = makeContext();
        const originalMeasure = ctx.measureText;
        ctx.measureText = function(text) {
            const m = originalMeasure.call(this, text);
            if (text.includes('…')) m.actualBoundingBoxAscent *= 4;
            return m;
        };
        const layout = fitOverlayLineText(ctx, entry('长'.repeat(30), {w: 40, h: 10 / OVERLAY_FONT_HEIGHT_FACTOR}));
        assert.equal(layout.display, '');
        assert.equal(layout.size, 0);
    });

    test('极窄框不把省略号画到框外，空文本与无效框返回空布局', () => {
        const h = Math.max(24, 24 / OVERLAY_FONT_HEIGHT_FACTOR);
        for (const e of [entry('长句', {w: 8, h}), entry(''), entry('字', {w: 0}), entry('字', {h: NaN})]) {
            const layout = fitOverlayLineText(makeContext(), e);
            assert.equal(layout.size, 0);
            assert.equal(layout.display, '');
        }
    });

    test('省略在字素边界截取，保留组合字符和完整 emoji', () => {
        const ctx = makeContext();
        const segmenter = new Intl.Segmenter(undefined, {granularity: 'grapheme'});
        ctx.measureText = function(text) {
            const size = parseFloat(this.font);
            return {width: [...segmenter.segment(text)].length * size};
        };
        for (const unit of ['e\u0301', '👨‍👩‍👧‍👦']) {
            const layout = fitOverlayLineText(ctx, entry(unit.repeat(20), {w: 40, h: Math.max(24, 24 / OVERLAY_FONT_HEIGHT_FACTOR)}));
            assert.equal(layout.display, unit.repeat(3) + '…');
        }
    });
});

describe('嵌图渲染接入（原文、译文与后台快照）', () => {
    test('原框完整遮罩，长句按新布局绘制，完整译文数据不变', async () => {
        globalThis.window = globalThis;
        globalThis.document = {getElementById: () => null};
        const {renderOverlaySnapshotTo} = await import('./annotation-engine.js');
        const entries = longLineEntries();
        for (const mode of ['source', 'translated']) {
            const ctx = makeContext();
            const masks = [], draws = [];
            ctx.fillRect = (...rect) => masks.push(rect);
            ctx.fillText = (text, x, y) => draws.push({text, x, y, size: parseFloat(ctx.font), baseline: ctx.textBaseline});
            const lines = entries.map(({line}) => ({...line, bgColor: 'rgb(255, 255, 255)', inkColor: 'rgb(0, 0, 0)'}));
            const expected = layoutOverlayText(ctx, entries);
            renderOverlaySnapshotTo({mode, lines, fontScale: 1}, ctx, 240, 100);
            assert.deepEqual(masks, entries.map(({r}) => [r.x, r.y, r.w, r.h]));
            assert.deepEqual(draws, expected.map(({display, x, y, size}) => ({text: display, x, y, size, baseline: 'alphabetic'})));
            assert.equal(lines[1].dstText, entries[1].text);
            assert.equal(ctx.font, '12px serif');
        }
    });

    test('彩色分段文字沿用同一字形基线与字号', async () => {
        const {renderOverlaySnapshotTo} = await import('./annotation-engine.js');
        const ctx = makeContext();
        const draws = [];
        ctx.fillRect = () => {};
        ctx.fillText = (text, x, y) => draws.push({text, x, y, font: ctx.font, baseline: ctx.textBaseline});
        const e = entry('甲乙');
        const line = {...e.line, bgColor: 'rgb(255, 255, 255)', inkColor: 'rgb(0, 0, 0)',
            inkSampled: {ink: 'rgb(0, 0, 0)', charInks: ['rgb(200, 0, 0)', 'rgb(0, 0, 0)'], inkConfidence: 1}};
        const expected = fitOverlayLineText(ctx, e);
        renderOverlaySnapshotTo({mode: 'source', lines: [line]}, ctx, 240, 100);
        assert.equal(draws.map((d) => d.text).join(''), e.text);
        assert.equal(draws.length, 2);
        for (const draw of draws) {
            assert.equal(draw.y, expected.y);
            assert.equal(parseFloat(draw.font), expected.size);
            assert.equal(draw.baseline, 'alphabetic');
        }
        assert.equal(draws[1].x, draws[0].x + expected.size);
    });
});
