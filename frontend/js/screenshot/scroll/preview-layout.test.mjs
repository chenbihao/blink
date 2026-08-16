import assert from 'node:assert/strict';
import {computePredictedLocatorTop, computePreviewPosition} from './preview-layout.js';

// ── 基本定位（单屏，origin 0,0 与旧 viewportWidth/Height 等价） ──────────

assert.deepEqual(computePreviewPosition({
    rect: {x: 100, y: 80, w: 500, h: 300},
    direction: 'vertical', monitorRect: {x: 0, y: 0, w: 1000, h: 700},
    previewWidth: 120, previewHeight: 400, gap: 8,
}), {left: 608, top: 80});

assert.deepEqual(computePreviewPosition({
    rect: {x: 500, y: 500, w: 450, h: 180},
    direction: 'vertical', monitorRect: {x: 0, y: 0, w: 1000, h: 700},
    previewWidth: 120, previewHeight: 600, gap: 8,
}), {left: 372, top: 92}, '右侧不足时放左侧，增高后仍完整留在屏幕内');

assert.deepEqual(computePreviewPosition({
    rect: {x: 20, y: 500, w: 900, h: 180},
    direction: 'horizontal', monitorRect: {x: 0, y: 0, w: 1000, h: 700},
    previewWidth: 120, previewHeight: 200, gap: 8,
}), {left: 20, top: 292}, '下方不足时移到选区上方');

// ── 副屏在主屏左侧（负坐标 origin） ──────────────────────────────────

// monitorRect = { x: -1920, y: 0, w: 1920, h: 1080 }
// 选区在副屏上
// rect.x=-1500, rect.w=500, gap=8 → right = -1500+500+8 = -992
// right + previewWidth = -992+120 = -872 <= mx+mw-gap = -8 → preferredLeft = right = -992
assert.deepEqual(computePreviewPosition({
    rect: {x: -1500, y: 100, w: 500, h: 300},
    direction: 'vertical', monitorRect: {x: -1920, y: 0, w: 1920, h: 1080},
    previewWidth: 120, previewHeight: 400, gap: 8,
}), {left: -992, top: 100}, '副屏左侧负坐标：预览贴选区右侧');

// 预览不应 clamp 到 0，而应 clamp 到副屏 origin
// rect.x=-1900, rect.w=100, gap=8 → right = -1900+100+8 = -1792
// right + previewWidth = -1792+120 = -1672 <= -8 → preferredLeft = right = -1792
// clamp(-1792, -1912, -8) → -1792
assert.deepEqual(computePreviewPosition({
    rect: {x: -1900, y: 100, w: 100, h: 300},
    direction: 'vertical', monitorRect: {x: -1920, y: 0, w: 1920, h: 1080},
    previewWidth: 120, previewHeight: 400, gap: 8,
}), {left: -1792, top: 100}, '预览 clamp 到副屏 origin 内侧，不回到 0');

// ── 副屏位于主屏上方（负 Y origin） ──────────────────────────────────

assert.deepEqual(computePreviewPosition({
    rect: {x: 100, y: -900, w: 500, h: 300},
    direction: 'horizontal', monitorRect: {x: 0, y: -1080, w: 1920, h: 1080},
    previewWidth: 120, previewHeight: 200, gap: 8,
}), {left: 100, top: -592}, '副屏在上方：横向预览贴选区下方');

// ── 两屏高度不同，副屏下方有虚拟桌面空洞 ────────────────────────────

// 主屏 1920x1080, 副屏 1920x800 在右侧
// 选区在副屏底部
const monShortRight = {x: 1920, y: 0, w: 1920, h: 800};
assert.ok(
    computePreviewPosition({
        rect: {x: 2200, y: 500, w: 400, h: 200},
        direction: 'vertical', monitorRect: monShortRight,
        previewWidth: 120, previewHeight: 300, gap: 8,
    }).top <= 800 - 300 - 8,
    '预览不超出矮副屏底部',
);

// ── computePredictedLocatorTop 不变 ──────────────────────────────────

assert.equal(computePredictedLocatorTop({
    currentTop: 300, pendingTop: null, direction: 1,
    lastAcceptedShift: 120, viewportHeight: 300,
}), 420, '优先沿用最近一次已确认位移');
assert.equal(computePredictedLocatorTop({
    currentTop: 300, pendingTop: 420, direction: 1,
    lastAcceptedShift: 120, viewportHeight: 300,
}), 540, '连续滚轮应累积预测但不修改真实坐标');
assert.equal(computePredictedLocatorTop({
    currentTop: 300, pendingTop: 540, direction: 1,
    lastAcceptedShift: 300, viewportHeight: 300,
}), 540, '预测范围必须限制在当前视口的 80% 内');

console.log('scroll preview layout tests passed');
