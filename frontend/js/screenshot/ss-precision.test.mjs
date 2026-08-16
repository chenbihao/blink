//! 0.20.6 像素精调纯函数测试。
//! 验证选区 1px 移动、边界钳制、resize 1:1 约束、准星步进。

import {applySquareResize, clampRectToBitmapBounds, moveCrosshair1px, moveRect1px,} from './ss-selection-geometry.js';

// ── 断言辅助 ──────────────────────────────────────────────────────────────

function assertEqual(actual, expected, msg) {
    const aStr = JSON.stringify(actual);
    const eStr = JSON.stringify(expected);
    if (aStr !== eStr) {
        throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
    }
    console.log(`✓ ${msg}`);
}

// ── moveRect1px ───────────────────────────────────────────────────────────

function testMoveRect1px_BasicMove() {
    console.log('\n=== moveRect1px: 基本移动 ===');
    const rect = {x: 100, y: 200, w: 50, h: 30};
    const bmp = {w: 1920, h: 1080};

    assertEqual(moveRect1px(rect, 1, 0, bmp.w, bmp.h), {x: 101, y: 200, w: 50, h: 30}, '右移 1px');
    assertEqual(moveRect1px(rect, -1, 0, bmp.w, bmp.h), {x: 99, y: 200, w: 50, h: 30}, '左移 1px');
    assertEqual(moveRect1px(rect, 0, 1, bmp.w, bmp.h), {x: 100, y: 201, w: 50, h: 30}, '下移 1px');
    assertEqual(moveRect1px(rect, 0, -1, bmp.w, bmp.h), {x: 100, y: 199, w: 50, h: 30}, '上移 1px');
}

function testMoveRect1px_ClampToBitmapBounds() {
    console.log('\n=== moveRect1px: 边界钳制 ===');
    const bmp = {w: 100, h: 100};

    // 选区紧贴左边界
    const leftRect = {x: 0, y: 10, w: 20, h: 20};
    assertEqual(moveRect1px(leftRect, -1, 0, bmp.w, bmp.h), {x: 0, y: 10, w: 20, h: 20}, '左边界左移不越界');

    // 选区紧贴右边界
    const rightRect = {x: 80, y: 10, w: 20, h: 20};
    assertEqual(moveRect1px(rightRect, 1, 0, bmp.w, bmp.h), {x: 80, y: 10, w: 20, h: 20}, '右边界右移不越界');

    // 选区紧贴上边界
    const topRect = {x: 10, y: 0, w: 20, h: 20};
    assertEqual(moveRect1px(topRect, 0, -1, bmp.w, bmp.h), {x: 10, y: 0, w: 20, h: 20}, '上边界上移不越界');

    // 选区紧贴下边界
    const bottomRect = {x: 10, y: 80, w: 20, h: 20};
    assertEqual(moveRect1px(bottomRect, 0, 1, bmp.w, bmp.h), {x: 10, y: 80, w: 20, h: 20}, '下边界下移不越界');
}

function testMoveRect1px_DoesNotChangeSize() {
    console.log('\n=== moveRect1px: 不改变宽高 ===');
    const rect = {x: 50, y: 50, w: 100, h: 80};
    const moved = moveRect1px(rect, 1, 1, 1000, 1000);
    assertEqual(moved.w, 100, '宽度不变');
    assertEqual(moved.h, 80, '高度不变');
}

// ── applySquareResize ─────────────────────────────────────────────────────

function testApplySquareResize_SE() {
    console.log('\n=== applySquareResize: SE handle (锚 NW) ===');
    const original = {x: 100, y: 100, w: 50, h: 30};
    const bmp = {w: 1920, h: 1080};
    // 拖 SE 到 newW=80, newH=60 → side=80 → 正方形 80×80，锚 NW
    const result = applySquareResize(original, 'se', 80, 60, bmp.w, bmp.h);
    assertEqual(result, {x: 100, y: 100, w: 80, h: 80}, 'SE → 80×80 锚 NW');
}

function testApplySquareResize_NW() {
    console.log('\n=== applySquareResize: NW handle (锚 SE) ===');
    const original = {x: 100, y: 100, w: 50, h: 30};
    const bmp = {w: 1920, h: 1080};
    // 拖 NW → 锚 SE = (150, 130)，side=80 → x=150-80=70, y=130-80=50
    const result = applySquareResize(original, 'nw', 80, 60, bmp.w, bmp.h);
    assertEqual(result, {x: 70, y: 50, w: 80, h: 80}, 'NW → 80×80 锚 SE');
}

function testApplySquareResize_ClampToBitmap() {
    console.log('\n=== applySquareResize: 钳制到 bitmap 边界 ===');
    const original = {x: 0, y: 0, w: 50, h: 50};
    const bmp = {w: 100, h: 100};
    // 拖 SE，side=200，但 bitmap 只有 100 → w=h=100
    const result = applySquareResize(original, 'se', 200, 200, bmp.w, bmp.h);
    assertEqual(result, {x: 0, y: 0, w: 100, h: 100}, 'SE 钳制到 100×100');
}

function testApplySquareResize_NegativeNewSize() {
    console.log('\n=== applySquareResize: 负方向 resize ===');
    const original = {x: 200, y: 200, w: 50, h: 50};
    const bmp = {w: 1000, h: 1000};
    // 拖 NW 向外，newW=-80（缩小方向）→ side=80
    const result = applySquareResize(original, 'nw', -80, -60, bmp.w, bmp.h);
    // side=max(80,60)=80, 锚 SE=(250,250) → x=250-80=170, y=250-80=170
    assertEqual(result, {x: 170, y: 170, w: 80, h: 80}, 'NW 负方向 → 80×80');
}

// ── moveCrosshair1px ──────────────────────────────────────────────────────

function testMoveCrosshair1px_BasicMove() {
    console.log('\n=== moveCrosshair1px: 基本移动 ===');
    const rect = {x: 100, y: 200, w: 50, h: 30};
    const pos = {x: 120, y: 210};

    assertEqual(moveCrosshair1px(pos, 1, 0, rect), {x: 121, y: 210}, '右移 1px');
    assertEqual(moveCrosshair1px(pos, -1, 0, rect), {x: 119, y: 210}, '左移 1px');
    assertEqual(moveCrosshair1px(pos, 0, 1, rect), {x: 120, y: 211}, '下移 1px');
    assertEqual(moveCrosshair1px(pos, 0, -1, rect), {x: 120, y: 209}, '上移 1px');
}

function testMoveCrosshair1px_ClampToSelection() {
    console.log('\n=== moveCrosshair1px: 钳制到选区 ===');
    const rect = {x: 100, y: 200, w: 50, h: 30};

    // 准星在选区左边界
    const leftPos = {x: 100, y: 210};
    assertEqual(moveCrosshair1px(leftPos, -1, 0, rect), {x: 100, y: 210}, '左边界左移不越界');

    // 准星在选区右边界（rect.x + rect.w - 1 = 149）
    const rightPos = {x: 149, y: 210};
    assertEqual(moveCrosshair1px(rightPos, 1, 0, rect), {x: 149, y: 210}, '右边界右移不越界');

    // 准星在选区上边界
    const topPos = {x: 120, y: 200};
    assertEqual(moveCrosshair1px(topPos, 0, -1, rect), {x: 120, y: 200}, '上边界上移不越界');

    // 准星在选区下边界（rect.y + rect.h - 1 = 229）
    const bottomPos = {x: 120, y: 229};
    assertEqual(moveCrosshair1px(bottomPos, 0, 1, rect), {x: 120, y: 229}, '下边界下移不越界');
}

// ── clampRectToBitmapBounds ───────────────────────────────────────────────

function testClampRectToBitmapBounds() {
    console.log('\n=== clampRectToBitmapBounds: 钳制到 bitmap 边界 ===');
    const bmpW = 1920, bmpH = 1080;

    // 正常矩形不变
    const normal = {x: 100, y: 200, w: 300, h: 200};
    const r1 = clampRectToBitmapBounds(normal, bmpW, bmpH);
    assertEqual(r1, {x: 100, y: 200, w: 300, h: 200}, '正常矩形不变');

    // 超出右下
    const overflow = {x: 1800, y: 1000, w: 300, h: 200};
    const r2 = clampRectToBitmapBounds(overflow, bmpW, bmpH);
    assertEqual(r2, {x: 1800, y: 1000, w: 120, h: 80}, '超出右下被钳制');

    // 超出左上
    const underflow = {x: -50, y: -50, w: 300, h: 200};
    const r3 = clampRectToBitmapBounds(underflow, bmpW, bmpH);
    assertEqual(r3, {x: 0, y: 0, w: 250, h: 150}, '超出左上被钳制');
}

// ── 主测试入口 ─────────────────────────────────────────────────────────────

function runAllTests() {
    console.log('\n🧪 开始 0.20.6 像素精调测试套件...\n');
    try {
        testMoveRect1px_BasicMove();
        testMoveRect1px_ClampToBitmapBounds();
        testMoveRect1px_DoesNotChangeSize();

        testApplySquareResize_SE();
        testApplySquareResize_NW();
        testApplySquareResize_ClampToBitmap();
        testApplySquareResize_NegativeNewSize();

        testMoveCrosshair1px_BasicMove();
        testMoveCrosshair1px_ClampToSelection();

        testClampRectToBitmapBounds();

        console.log('\n✅ 所有测试通过！');
        return true;
    } catch (e) {
        console.error('\n❌ 测试失败:', e.message);
        return false;
    }
}

if (typeof process !== 'undefined' && process.versions?.node) {
    const ok = runAllTests();
    if (!ok) process.exit(1);
}

export {runAllTests};
