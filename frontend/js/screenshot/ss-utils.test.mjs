//! ss-utils 纯函数测试（0.19.16）。
//!
//! 覆盖：
//! 1. computePanAxisBounds：单轴平移边界
//! 2. computeFloatingPlacement：浮动 UI 定位

import assert from 'node:assert/strict';
import { computePanAxisBounds, computeFloatingPlacement } from './ss-utils.js';

// ── computePanAxisBounds ─────────────────────────────────────────────────

// 图片小于视口：完整位于视口内
assert.deepEqual(
  computePanAxisBounds(800, 1920),
  { min: 0, max: 1120 },
  '图片小于视口: min=0, max=viewport-image',
);

// 图片等于视口：不能移动
assert.deepEqual(
  computePanAxisBounds(1080, 1080),
  { min: 0, max: 0 },
  '图片等于视口: min=0, max=0',
);

// 图片大于视口：默认 minVisible=48
assert.deepEqual(
  computePanAxisBounds(5000, 1920),
  { min: 48 - 5000, max: 1920 - 48 },
  '图片大于视口: min=minVisible-image, max=viewport-minVisible',
);
assert.equal(computePanAxisBounds(5000, 1920).min, -4952);
assert.equal(computePanAxisBounds(5000, 1920).max, 1872);

// 图片大于视口：自定义 minVisible（origin=0）
assert.deepEqual(
  computePanAxisBounds(5000, 1920, 0, 100),
  { min: 100 - 5000, max: 1920 - 100 },
  '自定义 minVisible=100',
);

// 初始位置 12 在大图边界范围内
{
  const bounds = computePanAxisBounds(5000, 1920);
  assert.ok(12 >= bounds.min, 'initialX=12 >= bounds.min');
  assert.ok(12 <= bounds.max, 'initialX=12 <= bounds.max');
}

// 初始位置 12 在小图边界范围内
{
  const bounds = computePanAxisBounds(800, 1920);
  const initialX = Math.round((1920 - 800) / 2);
  assert.ok(initialX >= bounds.min, 'centered initialX >= bounds.min');
  assert.ok(initialX <= bounds.max, 'centered initialX <= bounds.max');
}

// 非法输入：零
assert.deepEqual(computePanAxisBounds(0, 1920), { min: 0, max: 0 }, 'imageSize=0');
assert.deepEqual(computePanAxisBounds(800, 0), { min: 0, max: 0 }, 'viewportSize=0');

// 非法输入：负数
assert.deepEqual(computePanAxisBounds(-100, 1920), { min: 0, max: 0 }, 'imageSize<0');
assert.deepEqual(computePanAxisBounds(800, -1), { min: 0, max: 0 }, 'viewportSize<0');

// 非法输入：非有限数
assert.deepEqual(computePanAxisBounds(NaN, 1920), { min: 0, max: 0 }, 'imageSize=NaN');
assert.deepEqual(computePanAxisBounds(800, Infinity), { min: 0, max: 0 }, 'viewportSize=Infinity');

console.log('✓ computePanAxisBounds tests passed');

// ── computeFloatingPlacement ─────────────────────────────────────────────

const MON = { x: 0, y: 0, w: 1920, h: 1080 };

// 下方居中：空间充足
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 100, y: 100, w: 200, h: 150 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: MON, margin: 8,
  });
  assert.deepEqual(result, { left: 50, top: 258 }, '下方居中: left=centerX-W/2, top=anchorBottom+margin');
}

// 上方回退：下方不够但上方够
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 100, y: 1000, w: 200, h: 60 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: MON, margin: 8,
  });
  assert.equal(result.top, 952, '上方回退: top=anchorY-visualH-margin');
  assert.equal(result.left, 50, '上方回退: left 仍居中');
}

// 兜底：上下都不够
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 100, y: 400, w: 200, h: 400 },
    visualWidth: 300, visualHeight: 500,
    monitorRect: MON, margin: 8,
  });
  // belowTop = 808, +500=1308 > 1072
  // aboveTop = 400-500-8 = -108 < 8
  // fallback: max(8, min(400, 1080-500-8=572)) = 400
  assert.equal(result.top, 400, '兜底: top=anchorY clamped');
}

// 水平右 clamp
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 1800, y: 100, w: 200, h: 150 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: MON, margin: 8,
  });
  // centerX=1900, left=1750, maxLeft=1920-300-8=1612
  assert.equal(result.left, 1612, '水平右 clamp 到屏边');
}

// 水平左 clamp
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 0, y: 100, w: 100, h: 150 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: MON, margin: 8,
  });
  // centerX=50, left=-100, minLeft=8
  assert.equal(result.left, 8, '水平左 clamp 到屏边');
}

// 跨屏：monitorRect 不从原点开始
{
  const mon2 = { x: 1920, y: 0, w: 1920, h: 1080 };
  const result = computeFloatingPlacement({
    anchorRect: { x: 2000, y: 100, w: 200, h: 150 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: mon2, margin: 8,
  });
  // centerX=2100, left=1950, minLeft=1920+8=1928
  assert.equal(result.left, 1950, '副屏: left 在副屏范围内');
  assert.ok(result.left >= mon2.x + 8, '副屏: left >= monX+margin');
  assert.ok(result.left + 300 <= mon2.x + mon2.w - 8, '副屏: right <= monX+monW-margin');
}

// 自定义 margin
{
  const result = computeFloatingPlacement({
    anchorRect: { x: 100, y: 100, w: 200, h: 150 },
    visualWidth: 300, visualHeight: 40,
    monitorRect: MON, margin: 20,
  });
  assert.equal(result.top, 100 + 150 + 20, '自定义 margin: top=anchorBottom+margin');
}

console.log('✓ computeFloatingPlacement tests passed');

console.log('\nss-utils tests all passed');
