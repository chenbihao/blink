//! 0.23.12 空白放行 move：点到文字框距离计算测试
//!
//! 覆盖：
//! 1. `pointToRectDistance`：框内 / 四侧 / 斜角 / 负坐标
//! 2. `nearestTextDistance`：多框取最近、空数组返回 Infinity
//!
//! 语义背景：hit-canvas pointerdown 在 hitTestWord miss 且距离最近文字框
//! 超过阈值（READING_BLANK_MOVE_THRESHOLD_CSS = 10 CSS px，模块内常量）
//! 时放行为移动选区手势；阈值内仍走 nearestWordByLine 划词兜底。

import {describe, test} from 'node:test';
import assert from 'node:assert';

// Mock window before importing ss-reading.js (which imports api.js → tauri.js)
globalThis.window = globalThis;

const {pointToRectDistance, nearestTextDistance} = await import('./ss-reading.js');

describe('pointToRectDistance — 点到矩形最短距离', () => {
    const rect = {x: 100, y: 200, w: 50, h: 20}; // [100,150] × [200,220]

    test('点在框内返回 0', () => {
        assert.strictEqual(pointToRectDistance(120, 210, rect), 0);
        assert.strictEqual(pointToRectDistance(100, 200, rect), 0); // 左上角
        assert.strictEqual(pointToRectDistance(150, 220, rect), 0); // 右下角
    });

    test('点在框右侧：水平距离', () => {
        assert.strictEqual(pointToRectDistance(160, 210, rect), 10);
        assert.strictEqual(pointToRectDistance(150, 210, rect), 0); // 右边缘算在内
    });

    test('点在框左侧：水平距离', () => {
        assert.strictEqual(pointToRectDistance(90, 210, rect), 10);
    });

    test('点在框下方：垂直距离', () => {
        assert.strictEqual(pointToRectDistance(120, 230, rect), 10);
    });

    test('点在框上方：垂直距离', () => {
        assert.strictEqual(pointToRectDistance(120, 180, rect), 20);
    });

    test('点在框斜向：勾股距离', () => {
        // 右下角 (150,220) 外偏 (30,40) → 50
        assert.strictEqual(pointToRectDistance(180, 260, rect), 50);
    });

    test('负坐标域正确', () => {
        const r2 = {x: -50, y: -30, w: 20, h: 10}; // [-50,-30] × [-10,-20]
        assert.strictEqual(pointToRectDistance(-60, -40, r2), Math.hypot(10, 10));
        assert.strictEqual(pointToRectDistance(-40, -25, r2), 0);
    });

    test('退化矩形（w=h=0）按点处理', () => {
        const r0 = {x: 10, y: 10, w: 0, h: 0};
        assert.strictEqual(pointToRectDistance(13, 14, r0), 5); // 3-4-5
    });
});

describe('nearestTextDistance — 到最近文字框的距离', () => {
    test('空数组返回 Infinity（无文字恒为空白）', () => {
        assert.strictEqual(nearestTextDistance([], 0, 0), Infinity);
    });

    test('多框取最近', () => {
        const items = [
            {rect: {x: 0, y: 0, w: 40, h: 16}},
            {rect: {x: 200, y: 100, w: 40, h: 16}},
        ];
        // 到框1：dx=60（x 右侧）、dy=44（y 下方）→ hypot(60,44)；
        // 到框2：hypot(100,40) 更远——应取框1
        assert.strictEqual(nearestTextDistance(items, 100, 60), Math.hypot(60, 44));
    });

    test('点在某一框内返回 0', () => {
        const items = [
            {rect: {x: 0, y: 0, w: 40, h: 16}},
            {rect: {x: 200, y: 100, w: 40, h: 16}},
        ];
        assert.strictEqual(nearestTextDistance(items, 210, 108), 0);
    });

    test('行间窄缝场景：两行间距小于阈值', () => {
        // 行高 16、行距 6：两框 [0,0,40,16] 与 [0,22,40,16]
        const items = [
            {rect: {x: 0, y: 0, w: 40, h: 16}},
            {rect: {x: 0, y: 22, w: 40, h: 16}},
        ];
        // 行间中点 (20,19)：到上下框各 3px < 10px 阈值 → 划词而非 move
        assert.ok(nearestTextDistance(items, 20, 19) <= 10);
    });

    test('大空白场景：距离远超阈值', () => {
        const items = [
            {rect: {x: 0, y: 0, w: 40, h: 16}},
        ];
        // 下方 80px 处的空白 → 80 > 10 → 放行 move
        assert.ok(nearestTextDistance(items, 20, 96) > 10);
    });
});
