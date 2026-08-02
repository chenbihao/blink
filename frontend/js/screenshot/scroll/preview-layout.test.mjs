import assert from 'node:assert/strict';
import { computePreviewPosition } from './preview-layout.js';

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

console.log('scroll preview layout tests passed');
