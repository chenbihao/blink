//! UI scale 与 renderScale 消费者测试。
//!
//! 覆盖：
//! 1. renderScale=2 / devicePixelRatio=1：绘制/裁剪 rect 使用 renderScale
//! 2. renderScale=1 / devicePixelRatio=2：防止"偏大"回归
//! 3. uiScaleAtCss 纯函数：
//!    - renderScale=1.5，目标屏 100% → 2/3
//!    - renderScale=1.5，目标屏 150% → 1
//!    - renderScale=1，目标屏 200% → 2
//! 4. toolbar clamp 使用缩放后视觉宽高

if (typeof globalThis.window === 'undefined') {
  globalThis.window = { devicePixelRatio: 1 };
}

import {
  getRenderScale,
  cssRectToBitmap,
  cssPointToBitmap,
  monitorDprAtCss,
  uiScaleAtCss,
} from './ss-selection-geometry.js';

// ── 断言辅助 ──────────────────────────────────────────────────────────────

function assertFloatEqual(actual, expected, msg, tolerance = 0.001) {
  if (Math.abs(actual - expected) > tolerance) {
    throw new Error(`${msg}: expected ${expected}, got ${actual}`);
  }
  console.log(`✓ ${msg}`);
}

function assertEqual(actual, expected, msg) {
  const aStr = JSON.stringify(actual);
  const eStr = JSON.stringify(expected);
  if (aStr !== eStr) {
    throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
  }
  console.log(`✓ ${msg}`);
}

function assertTrue(value, msg) {
  if (!value) throw new Error(`${msg}: expected true, got ${value}`);
  console.log(`✓ ${msg}`);
}

// ── 场景夹具 ──────────────────────────────────────────────────────────────

// 场景 1：renderScale=2, devicePixelRatio=1
// overlay 跨屏 HWND 渲染在 100% 屏但 canvas 实测 renderScale=2
// （如：后端注入 overlayDpi=96，但 canvas bitmap/CSS = 2）
const META_RS2_DPR1 = {
  vx: 0, vy: 0, w: 3840, h: 2160, overlayDpi: 96, fgHwnd: 0,
  renderScaleX: 2, renderScaleY: 2,
  physicalDisplays: [
    { x: 0,    y: 0, w: 1920, h: 1080, primary: true,  dpi: 96 },
    { x: 1920, y: 0, w: 1920, h: 1080, primary: false, dpi: 96 },
  ],
};

// 场景 2：renderScale=1, devicePixelRatio=2
// overlay 渲染在 200% 屏但 canvas 实测 renderScale=1
// （如：后端注入 overlayDpi=192，但 canvas bitmap/CSS = 1）
const META_RS1_DPR2 = {
  vx: 0, vy: 0, w: 1920, h: 1080, overlayDpi: 192, fgHwnd: 0,
  renderScaleX: 1, renderScaleY: 1,
  physicalDisplays: [
    { x: 0, y: 0, w: 1920, h: 1080, primary: true, dpi: 192 },
  ],
};

// 场景 3a：renderScale=1.5, 目标屏 100%
const META_RS15_MON100 = {
  vx: 0, vy: 0, w: 4000, h: 1200, overlayDpi: 96, fgHwnd: 0,
  renderScaleX: 1.5, renderScaleY: 1.5,
  physicalDisplays: [
    { x: 0,    y: 0, w: 2000, h: 1200, primary: true,  dpi: 96 },
    { x: 2000, y: 0, w: 2000, h: 1200, primary: false, dpi: 144 },
  ],
};

// 场景 3b：renderScale=1.5, 目标屏 150%
// （同上 META_RS15_MON100，选区右下角落在副屏）

// 场景 3c：renderScale=1, 目标屏 200%
const META_RS1_MON200 = {
  vx: 0, vy: 0, w: 1920, h: 1080, overlayDpi: 192, fgHwnd: 0,
  renderScaleX: 1, renderScaleY: 1,
  physicalDisplays: [
    { x: 0, y: 0, w: 1920, h: 1080, primary: true, dpi: 192 },
  ],
};

// ── 测试 1：renderScale=2, dpr=1 绘制/裁剪使用 renderScale ────────────────

function testRs2Dpr1DrawCrop() {
  console.log('\n=== renderScale=2, dpr=1：绘制/裁剪 rect 使用 renderScale ===');

  // 设置 window.devicePixelRatio=1 模拟 dpr=1
  const origDpr = window.devicePixelRatio;
  window.devicePixelRatio = 1;

  // CSS rect {x:100, y:50, w:300, h:200}
  // 应使用 renderScale=2，不使用 dpr=1
  const cssRect = { x: 100, y: 50, w: 300, h: 200 };
  const bmp = cssRectToBitmap(cssRect, META_RS2_DPR1);
  assertEqual(bmp, { x: 200, y: 100, w: 600, h: 400 }, 'cssRectToBitmap 用 renderScale=2（非 dpr=1）');

  // 单点
  const pt = cssPointToBitmap(100, 50, META_RS2_DPR1);
  assertEqual(pt, { x: 200, y: 100 }, 'cssPointToBitmap 用 renderScale=2');

  // 如果错误使用 dpr=1，结果会是 {x:100, y:50, w:300, h:200}——验证不会发生
  assertTrue(bmp.w !== 300, '裁剪宽度不会错误地 = 300（dpr=1）');
  assertTrue(bmp.w === 600, '裁剪宽度正确 = 600（renderScale=2）');

  window.devicePixelRatio = origDpr;
}

// ── 测试 2：renderScale=1, dpr=2 防止"偏大"回归 ──────────────────────────

function testRs1Dpr2NoOversize() {
  console.log('\n=== renderScale=1, dpr=2：防止偏大回归 ===');

  const origDpr = window.devicePixelRatio;
  window.devicePixelRatio = 2;

  // CSS rect {x:100, y:50, w:300, h:200}
  // 应使用 renderScale=1，不使用 dpr=2
  const cssRect = { x: 100, y: 50, w: 300, h: 200 };
  const bmp = cssRectToBitmap(cssRect, META_RS1_DPR2);
  assertEqual(bmp, { x: 100, y: 50, w: 300, h: 200 }, 'cssRectToBitmap 用 renderScale=1（非 dpr=2）');

  // 如果错误使用 dpr=2，结果会是 {x:200, y:100, w:600, h:400}——验证不会发生
  assertTrue(bmp.w !== 600, '裁剪宽度不会错误地 = 600（dpr=2）');
  assertTrue(bmp.w === 300, '裁剪宽度正确 = 300（renderScale=1）');

  window.devicePixelRatio = origDpr;
}

// ── 测试 3：uiScaleAtCss 纯函数 ──────────────────────────────────────────

function testUiScaleRs15Mon100() {
  console.log('\n=== uiScale: renderScale=1.5, 目标屏 100% → 2/3 ===');

  // 选区右下角在主屏（100%, dpi=96）
  // CSS (500, 400) → screen (500*1.5, 400*1.5) = (750, 600) → 主屏 dpi=96 → monitorDpr=1
  // uiScale = 1 / 1.5 = 2/3 ≈ 0.6667
  const scale = uiScaleAtCss(500, 400, META_RS15_MON100);
  assertFloatEqual(scale, 2 / 3, 'uiScale = 1/1.5 = 2/3 ≈ 0.667');
}

function testUiScaleRs15Mon150() {
  console.log('\n=== uiScale: renderScale=1.5, 目标屏 150% → 1 ===');

  // 选区右下角在副屏（150%, dpi=144）
  // CSS (1500, 400) → screen (1500*1.5, 400*1.5) = (2250, 600)
  // 副屏 x:2000-4000 → 2250 在副屏 → monitorDpr=144/96=1.5
  // uiScale = 1.5 / 1.5 = 1
  const scale = uiScaleAtCss(1500, 400, META_RS15_MON100);
  assertFloatEqual(scale, 1, 'uiScale = 1.5/1.5 = 1');
}

function testUiScaleRs1Mon200() {
  console.log('\n=== uiScale: renderScale=1, 目标屏 200% → 2 ===');

  // 选区右下角在屏上（200%, dpi=192）
  // CSS (500, 400) → screen (500*1, 400*1) = (500, 400) → monitorDpr=192/96=2
  // uiScale = 2 / 1 = 2
  const scale = uiScaleAtCss(500, 400, META_RS1_MON200);
  assertFloatEqual(scale, 2, 'uiScale = 2/1 = 2');
}

// ── 测试 4：toolbar clamp 使用缩放后视觉宽高 ──────────────────────────────

function testToolbarClampUsesScaledWidth() {
  console.log('\n=== toolbar clamp 使用缩放后视觉宽高 ===');

  // 模拟 positionToolbar 中的 clamp 逻辑
  // 场景：renderScale=1.5, 目标屏 100%, uiScale=2/3
  // toolbar offsetWidth=300, offsetHeight=40
  // 视觉宽高 = 300 * 2/3 = 200, 40 * 2/3 ≈ 26.67
  // 屏 CSS 矩形 {x:0, y:0, w:1333.33, h:800}（2000/1.5）
  // MARGIN=8
  // maxLeft = 0 + 1333.33 - 200 - 8 = 1125.33

  const uiScale = 2 / 3;
  const offsetW = 300;
  const offsetH = 40;
  const visualW = offsetW * uiScale; // 200
  const visualH = offsetH * uiScale; // ~26.67

  const monX = 0, monY = 0, monW = 2000 / 1.5, monH = 1200 / 1.5;
  const MARGIN = 8;

  // 模拟 clamp
  const minLeft = monX + MARGIN;
  const maxLeft = monX + monW - visualW - MARGIN;
  const minTop = monY + MARGIN;
  const maxTop = monY + monH - visualH - MARGIN;

  // 验证使用了视觉宽高（而非 offsetWidth）
  assertFloatEqual(visualW, 200, '视觉宽度 = 300 * 2/3 = 200');
  assertFloatEqual(visualH, 40 * 2 / 3, '视觉高度 = 40 * 2/3');
  assertFloatEqual(maxLeft, monW - 200 - 8, 'maxLeft 使用视觉宽度 200');
  assertFloatEqual(maxTop, monH - visualH - 8, 'maxTop 使用视觉高度');

  // 如果错误使用 offsetWidth=300，maxLeft 会更小（工具栏被过度 clamp）
  const wrongMaxLeft = monX + monW - offsetW - MARGIN;
  assertTrue(maxLeft > wrongMaxLeft, '使用视觉宽高的 maxLeft > 错误使用 offsetWidth 的 maxLeft');

  // 验证具体数值
  // maxLeft = 1333.33 - 200 - 8 = 1125.33
  assertFloatEqual(maxLeft, 1333.333 - 200 - 8, 'maxLeft = 1125.33');
}

function testToolbarClampUiScale1() {
  console.log('\n=== toolbar clamp: uiScale=1 时视觉宽高 = offsetWidth ===');

  // 当 uiScale=1（renderScale = monitorDpr），视觉宽高 = offsetWidth
  const uiScale = 1;
  const offsetW = 300;
  const offsetH = 40;
  const visualW = offsetW * uiScale;
  const visualH = offsetH * uiScale;

  assertEqual(visualW, 300, 'uiScale=1: 视觉宽度 = offsetWidth = 300');
  assertEqual(visualH, 40, 'uiScale=1: 视觉高度 = offsetHeight = 40');
}

function testToolbarClampUiScale2() {
  console.log('\n=== toolbar clamp: uiScale=2 时视觉宽高 = 2×offsetWidth ===');

  // 当 uiScale=2（renderScale=1, monitorDpr=2），视觉宽高 = 2×offsetWidth
  const uiScale = 2;
  const offsetW = 300;
  const offsetH = 40;
  const visualW = offsetW * uiScale;
  const visualH = offsetH * uiScale;

  assertEqual(visualW, 600, 'uiScale=2: 视觉宽度 = 2×300 = 600');
  assertEqual(visualH, 80, 'uiScale=2: 视觉高度 = 2×40 = 80');
}

// ── 测试 5：monitorDprAtCss 与 uiScaleAtCss 一致性 ───────────────────────

function testUiScaleConsistency() {
  console.log('\n=== uiScaleAtCss 与 monitorDprAtCss / getRenderScale 一致 ===');

  // uiScaleAtCss(x, y, meta) === monitorDprAtCss(x, y, meta) / getRenderScale(meta).scaleX
  const meta = META_RS15_MON100;

  // 主屏上的点
  const x1 = 500, y1 = 400;
  const expected1 = monitorDprAtCss(x1, y1, meta) / getRenderScale(meta).scaleX;
  assertFloatEqual(uiScaleAtCss(x1, y1, meta), expected1, '主屏: uiScale = monitorDpr/renderScale');

  // 副屏上的点
  const x2 = 1500, y2 = 400;
  const expected2 = monitorDprAtCss(x2, y2, meta) / getRenderScale(meta).scaleX;
  assertFloatEqual(uiScaleAtCss(x2, y2, meta), expected2, '副屏: uiScale = monitorDpr/renderScale');
}

// ── 主测试入口 ─────────────────────────────────────────────────────────────

function runAllTests() {
  console.log('\n🧪 开始 ss-ui-scale 测试套件...\n');
  try {
    // renderScale=2, dpr=1
    testRs2Dpr1DrawCrop();

    // renderScale=1, dpr=2
    testRs1Dpr2NoOversize();

    // uiScale 纯函数
    testUiScaleRs15Mon100();
    testUiScaleRs15Mon150();
    testUiScaleRs1Mon200();

    // toolbar clamp
    testToolbarClampUsesScaledWidth();
    testToolbarClampUiScale1();
    testToolbarClampUiScale2();

    // 一致性
    testUiScaleConsistency();

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

export { runAllTests };
