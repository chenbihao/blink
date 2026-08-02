import assert from 'node:assert/strict';
import { computePredictedLocatorTop, computePreviewPosition } from './preview-layout.js';

assert.deepEqual(computePreviewPosition({
  rect: { x: 100, y: 80, w: 500, h: 300 },
  direction: 'vertical', viewportWidth: 1000, viewportHeight: 700,
  previewWidth: 120, previewHeight: 400, gap: 8,
}), { left: 608, top: 80 });

assert.deepEqual(computePreviewPosition({
  rect: { x: 500, y: 500, w: 450, h: 180 },
  direction: 'vertical', viewportWidth: 1000, viewportHeight: 700,
  previewWidth: 120, previewHeight: 600, gap: 8,
}), { left: 372, top: 92 }, '右侧不足时放左侧，增高后仍完整留在屏幕内');

assert.deepEqual(computePreviewPosition({
  rect: { x: 20, y: 500, w: 900, h: 180 },
  direction: 'horizontal', viewportWidth: 1000, viewportHeight: 700,
  previewWidth: 120, previewHeight: 200, gap: 8,
}), { left: 20, top: 292 }, '下方不足时移到选区上方');

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
