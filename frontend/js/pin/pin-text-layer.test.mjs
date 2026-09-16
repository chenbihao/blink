//! 0.23.12 pin 文字图层：items 构造与纯函数复用测试
//!
//! 覆盖：
//! 1. `sourceItemsFromOcrResult`：char_boxes 字符级优先 / word 降级 / 空结果
//! 2. `translatedItemsFromLines`：译文文本优先、回退原文、行号即 lineIndex
//! 3. 空白放行语义在 pin 坐标模型（imgScale ≠ 1）下的距离判定：
//!    阈值 BLANK_RELEASE_THRESHOLD_CSS 是视觉 CSS 距离，图片像素域按
//!    1/imgScale 折算（zoom 放大时间距缩小、缩小时扩大）
//!
//! DOM 事件绑定（pointerdown 划词 / 空白放行 startDragging）为薄事件层，
//! 不在本测试范围；核心判定逻辑由 shared/text-selection 测试覆盖。

import {describe, test} from 'node:test';
import assert from 'node:assert';

globalThis.window = globalThis;

const {
    sourceItemsFromOcrResult,
    translatedItemsFromLines,
} = await import('./pin-text-layer.js');
const {
    BLANK_RELEASE_THRESHOLD_CSS,
    hitTestItems,
    nearestItemByLine,
    nearestTextDistance,
    selectionTextOf,
} = await import('../shared/text-selection.js');

describe('sourceItemsFromOcrResult — 原文 items 构造', () => {
    test('char_boxes 优先：字符级，lineIndex 取 line_index', () => {
        const result = {
            text: '你好世界',
            words: [{text: '你好世界', rect: {x: 0, y: 0, w: 100, h: 20}, line_index: 0}],
            char_boxes: [
                {text: '你', rect: {x: 0, y: 0, w: 25, h: 20}, line_index: 0, char_start: 0, char_end: 1},
                {text: '好', rect: {x: 25, y: 0, w: 25, h: 20}, line_index: 0, char_start: 1, char_end: 2},
            ],
        };
        const items = sourceItemsFromOcrResult(result);
        assert.strictEqual(items.length, 2);
        assert.deepStrictEqual(items[0], {
            text: '你', rect: {x: 0, y: 0, w: 25, h: 20}, lineIndex: 0,
            start: 0, end: 1,
        });
        assert.strictEqual(items[1].lineIndex, 0);
    });

    test('无 char_boxes 降级 word 轨', () => {
        const result = {
            text: 'hello world',
            words: [
                {text: 'hello', rect: {x: 0, y: 0, w: 40, h: 16}, line_index: 0},
                {text: 'world', rect: {x: 48, y: 0, w: 40, h: 16}, line_index: 0},
            ],
            char_ranges: [[0, 5], [6, 11]],
        };
        const items = sourceItemsFromOcrResult(result);
        assert.strictEqual(items.length, 2);
        assert.strictEqual(items[0].text, 'hello');
        assert.strictEqual(items[1].lineIndex, 0);
        assert.strictEqual(selectionTextOf(items, 0, 1, result.text), 'hello world');
    });

    test('空结果返回空数组', () => {
        assert.deepStrictEqual(sourceItemsFromOcrResult({}), []);
        assert.deepStrictEqual(sourceItemsFromOcrResult(null), []);
    });
});

describe('translatedItemsFromLines — 译文 items 构造', () => {
    test('译文优先、原文回退、行号即 lineIndex', () => {
        const lines = [
            {rect: {x: 0, y: 0, w: 100, h: 20}, srcText: '你好', dstText: 'hello'},
            {rect: {x: 0, y: 30, w: 100, h: 20}, srcText: '世界', dstText: null},
        ];
        const items = translatedItemsFromLines(lines);
        assert.strictEqual(items.length, 2);
        assert.strictEqual(items[0].text, 'hello');
        assert.strictEqual(items[1].text, '世界'); // dstText null → srcText
        assert.strictEqual(items[0].lineIndex, 0);
        assert.strictEqual(items[1].lineIndex, 1);
        assert.strictEqual(selectionTextOf(items, 0, 1, 'hello\n世界'), 'hello\n世界');
    });

    test('dstText 与 srcText 均空时 text 为空串（不炸）', () => {
        const items = translatedItemsFromLines([{rect: {x: 0, y: 0, w: 10, h: 10}}]);
        assert.strictEqual(items[0].text, '');
    });
});

describe('pin 坐标模型下的空白放行与划词语义', () => {
    // 模拟：图片 400×200 资源像素，显示 CSS 200×100 → imgScale = 0.5
    const imgScale = 0.5;
    const items = [
        {rect: {x: 100, y: 100, w: 200, h: 32}, text: '你好世界', lineIndex: 0},
    ];
    // 视觉阈值折算到图片像素域：10 CSS px / 0.5 = 20 图片 px
    const thresholdImage = BLANK_RELEASE_THRESHOLD_CSS / imgScale;

    test('阈值折算：zoom 缩小（imgScale<1）时图片域阈值放大', () => {
        assert.strictEqual(thresholdImage, 20);
    });

    test('图片下方 100px（视觉 50px）处 → 真空白放行拖动', () => {
        const py = 100 + 32 + 100; // 框下边缘 + 100 图片 px
        assert.ok(nearestTextDistance(items, 200, py) > thresholdImage);
    });

    test('框下 5 图片 px（视觉 2.5px，行间窄缝）→ 仍划词', () => {
        const py = 100 + 32 + 5;
        assert.ok(nearestTextDistance(items, 200, py) <= thresholdImage);
    });

    test('命中判定与选择文本拼接', () => {
        const chars = [
            {rect: {x: 100, y: 100, w: 8, h: 32}, text: '你', lineIndex: 0},
            {rect: {x: 108, y: 100, w: 8, h: 32}, text: '好', lineIndex: 0},
            {rect: {x: 116, y: 100, w: 8, h: 32}, text: '世', lineIndex: 0},
            {rect: {x: 124, y: 100, w: 8, h: 32}, text: '界', lineIndex: 0},
        ];
        assert.strictEqual(hitTestItems(chars, 110, 110), 1);
        assert.strictEqual(hitTestItems(chars, 300, 110), -1);
        assert.strictEqual(selectionTextOf(chars, 1, 2), '好世');
        // 空白窄缝点击 → 最近行最近字符（编辑器手感）
        assert.strictEqual(nearestItemByLine(chars, 130, 150), 3);
    });

    test('selectionTextOf：空/倒序区间', () => {
        assert.strictEqual(selectionTextOf(items, 0, -1), '');
        assert.strictEqual(selectionTextOf(items, 0, 0), '你好世界');
    });
});
