//! 0.15.8 选区体验增强：坐标契约测试。
//! 0.18.8 更新：函数签名去 dpr 参数，改为 per-monitor 按屏查 dpr。
//!
//! 覆盖场景：
//! - 单屏 100%（无 displays → fallback overlay dpr）
//! - 左侧/上方负坐标副屏
//! - 窗口部分越界
//! - CSS ↔ bitmap 往返换算
//! - 统一格式化
//! - 0.18.8 per-monitor：dprAtScreen / dprAtCss / cssSizeToPhysical / 同 DPI 零回归 / 边界归属

// Node.js 环境下 mock window（per-monitor 函数 fallback 需要 window.devicePixelRatio）
if (typeof globalThis.window === 'undefined') {
  globalThis.window = { devicePixelRatio: 1 };
}

import {
  screenToBitmap, bitmapToScreen, screenToCss, cssToScreen,
  bitmapToCss, cssToBitmap, rectScreenToBitmap, rectBitmapToScreen,
  rectScreenToCss, rectCssToScreen, clampRectToBitmap, clampRectToCss,
  pointInRect, distanceCss, formatSelectionInfo, formatColor, rgbToHsl,
  dprAtScreen, dprAtCss, cssSizeToPhysical,
} from './ss-selection-geometry.js';

/** 测试辅助：浮点数比较 */
function assertFloatEqual(actual, expected, msg, tolerance = 0.001) {
  if (Math.abs(actual - expected) > tolerance) {
    throw new Error(`${msg}: expected ${expected}, got ${actual}`);
  }
  console.log(`✓ ${msg}`);
}

/** 测试辅助：对象比较 */
function assertEqual(actual, expected, msg) {
  const aStr = JSON.stringify(actual);
  const eStr = JSON.stringify(expected);
  if (aStr !== eStr) {
    throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
  }
  console.log(`✓ ${msg}`);
}

/** 测试辅助：布尔断言 */
function assertTrue(value, msg) {
  if (!value) {
    throw new Error(`${msg}: expected true, got ${value}`);
  }
  console.log(`✓ ${msg}`);
}

/** 测试辅助：布尔断言（false） */
function assertFalse(value, msg) {
  if (value) {
    throw new Error(`${msg}: expected false, got ${value}`);
  }
  console.log(`✓ ${msg}`);
}

// ── 0.18.8 per-monitor 测试 ──────────────────────────────────────────────

/**
 * 混合 DPI 双屏 mock meta：
 * 主屏 100% (96dpi, 物理1920×1080) + 副屏 150% (144dpi, 物理2560×1440)
 *
 * build_displays_json 每屏用各自 dpr 折算 CSS：
 * - 主屏: css_x = 0/1.0 = 0, css_w = 1920/1.0 = 1920
 * - 副屏: css_x = 1920/1.5 = 1280, css_w = 2560/1.5 ≈ 1707
 *
 * dprAtScreen 还原物理坐标（css * dpr + vx）：
 * - 主屏: physX = 0*1.0 = 0, physW = 1920*1.0 = 1920 → [0, 1920)
 * - 副屏: physX = 1280*1.5 = 1920, physW = 1707*1.5 = 2560.5 → [1920, 4480.5)
 */
const MIXED_DPI_META = {
  vx: 0, vy: 0, w: 4480, h: 1440, fgHwnd: 0,
  displays: [
    { x: 0,    y: 0, w: 1920, h: 1080, primary: true,  dpi: 96  },  // 100% 屏 dpr=1.0
    { x: 1280, y: 0, w: 1707, h: 960,  primary: false, dpi: 144 },  // 150% 屏 dpr=1.5
  ]
};

/** 同 DPI 双屏 mock meta（双 100%）：零回归验证用 */
const SAME_DPI_META = {
  vx: 0, vy: 0, w: 3840, h: 1080, fgHwnd: 0,
  displays: [
    { x: 0,    y: 0, w: 1920, h: 1080, primary: true,  dpi: 96 },
    { x: 1920, y: 0, w: 1920, h: 1080, primary: false, dpi: 96 },
  ]
};

/** 无 displays 的降级 meta（单屏场景，fallback overlay dpr） */
const NO_DISPLAYS_META = { vx: 0, vy: 0, w: 1920, h: 1080 };

function testDprAtScreen() {
  console.log('\n=== 0.18.8 dprAtScreen ===');

  // 100% 屏物理坐标 → dpr 1.0
  assertFloatEqual(dprAtScreen(100, 200, MIXED_DPI_META), 1.0, 'dprAtScreen 100% 屏 → 1.0');

  // 150% 屏物理坐标 → dpr 1.5
  // 副屏物理范围 [1920, 4480.5)，坐标 2000 在其中
  assertFloatEqual(dprAtScreen(2000, 200, MIXED_DPI_META), 1.5, 'dprAtScreen 150% 屏 → 1.5');

  // 屏外坐标 → fallback
  const fallback = dprAtScreen(99999, 99999, MIXED_DPI_META);
  assertFloatEqual(fallback, 1, 'dprAtScreen 屏外 → fallback 1（mock dpr）');

  // 无 displays meta → fallback
  const noDisp = dprAtScreen(100, 200, NO_DISPLAYS_META);
  assertFloatEqual(noDisp, 1, 'dprAtScreen 无 displays → fallback 1（mock dpr）');
}

function testDprAtCss() {
  console.log('\n=== 0.18.8 dprAtCss ===');

  // 100% 屏 CSS 坐标 → dpr 1.0
  assertFloatEqual(dprAtCss(100, 200, MIXED_DPI_META), 1.0, 'dprAtCss 100% 屏 → 1.0');

  // 150% 屏 CSS 坐标 → dpr 1.5
  // 副屏 CSS 范围 [1280, 2987)，坐标 2000 在其中
  assertFloatEqual(dprAtCss(2000, 200, MIXED_DPI_META), 1.5, 'dprAtCss 150% 屏 → 1.5');

  // 屏外坐标 → fallback
  const fallback = dprAtCss(99999, 99999, MIXED_DPI_META);
  assertFloatEqual(fallback, 1, 'dprAtCss 屏外 → fallback 1（mock dpr）');

  // 无 displays meta → fallback
  const noDisp = dprAtCss(100, 200, NO_DISPLAYS_META);
  assertFloatEqual(noDisp, 1, 'dprAtCss 无 displays → fallback 1（mock dpr）');
}

function testDprAtCssBoundaryDeterminism() {
  console.log('\n=== 0.18.8 dprAtCss 边界归属确定性 ===');

  // CSS 边界 x=1280：主屏右边界（排他，不含）与副屏左边界（含）重合
  // 主屏 [0, 1920) 包含 1280，副屏 [1280, 2987) 也包含 1280
  // "靠后命中"规则取副屏 dpr=1.5
  const boundaryDpr = dprAtCss(1280, 0, MIXED_DPI_META);
  assertFloatEqual(boundaryDpr, 1.5, 'dprAtCss 边界 x=1280 → 副屏 dpr 1.5（靠后命中）');

  // 多次调用结果一致（无抖动）
  for (let i = 0; i < 5; i++) {
    const dpr = dprAtCss(1280, 0, MIXED_DPI_META);
    assertFloatEqual(dpr, 1.5, `dprAtCss 边界第 ${i+1} 次调用一致`);
  }
}

function testScreenToCssPerMonitor() {
  console.log('\n=== 0.18.8 screenToCss per-monitor 分段换算 ===');

  // 100% 屏物理坐标 (100, 200) → CSS (100, 200)（dpr=1.0）
  const css1 = screenToCss(100, 200, MIXED_DPI_META);
  assertFloatEqual(css1.x, 100, 'screenToCss 100% 屏 x');
  assertFloatEqual(css1.y, 200, 'screenToCss 100% 屏 y');

  // 150% 屏物理坐标 (1920, 0) → CSS (1280, 0)（dpr=1.5）
  const css2 = screenToCss(1920, 0, MIXED_DPI_META);
  assertFloatEqual(css2.x, 1280, 'screenToCss 150% 屏 x (物理1920 → CSS 1280)');
  assertFloatEqual(css2.y, 0, 'screenToCss 150% 屏 y');

  // 往返无损：CSS (1280, 0) → 物理 (1920, 0)
  const screen1 = cssToScreen(1280, 0, MIXED_DPI_META);
  assertEqual(screen1, { x: 1920, y: 0 }, 'cssToScreen 往返 (CSS 1280 → 物理 1920)');
}

function testSameDpiZeroRegression() {
  console.log('\n=== 0.18.8 同 DPI 双屏零回归 ===');

  // 同 DPI 双屏（双 100%）：per-monitor 换算结果应与单一 dpr=1.0 一致
  const meta = SAME_DPI_META;

  // 主屏物理坐标 → CSS（dpr=1.0）
  const css1 = screenToCss(100, 200, meta);
  assertFloatEqual(css1.x, 100, '同 DPI 主屏 screenToCss x');
  assertFloatEqual(css1.y, 200, '同 DPI 主屏 screenToCss y');

  // 副屏物理坐标 → CSS（dpr=1.0）
  const css2 = screenToCss(2000, 300, meta);
  assertFloatEqual(css2.x, 2000, '同 DPI 副屏 screenToCss x');
  assertFloatEqual(css2.y, 300, '同 DPI 副屏 screenToCss y');

  // CSS → 物理（dpr=1.0）
  const screen1 = cssToScreen(100, 200, meta);
  assertEqual(screen1, { x: 100, y: 200 }, '同 DPI cssToScreen 主屏');

  const screen2 = cssToScreen(2000, 300, meta);
  assertEqual(screen2, { x: 2000, y: 300 }, '同 DPI cssToScreen 副屏');

  // 矩阵版零回归
  const rect = { x: 50, y: 60, w: 200, h: 100 };
  const rectCss = rectScreenToCss(rect, meta);
  assertFloatEqual(rectCss.x, 50, '同 DPI rectScreenToCss x');
  assertFloatEqual(rectCss.y, 60, '同 DPI rectScreenToCss y');
  assertFloatEqual(rectCss.w, 200, '同 DPI rectScreenToCss w');
  assertFloatEqual(rectCss.h, 100, '同 DPI rectScreenToCss h');
}

function testCssSizeToPhysical() {
  console.log('\n=== 0.18.8 cssSizeToPhysical ===');

  // 100% 屏：200px CSS → 200px 物理尺寸
  const phys1 = cssSizeToPhysical(200, 100, 100, MIXED_DPI_META);
  assertEqual(phys1, 200, 'cssSizeToPhysical 100% 屏 200px → 200px');

  // 150% 屏：200px CSS → 300px 物理尺寸
  const phys2 = cssSizeToPhysical(200, 2000, 200, MIXED_DPI_META);
  assertEqual(phys2, 300, 'cssSizeToPhysical 150% 屏 200px → 300px');

  // 同 DPI 双屏：200px → 200px（零回归）
  const phys3 = cssSizeToPhysical(200, 100, 100, SAME_DPI_META);
  assertEqual(phys3, 200, 'cssSizeToPhysical 同 DPI 200px → 200px');
}

// ── 原有测试（0.18.8 签名更新） ──────────────────────────────────────────

function testSingleScreen100() {
  console.log('\n=== 单屏 100% (无 displays, fallback dpr) ===');
  // 无 displays → fallback window.devicePixelRatio || 1（mock = 1）
  const meta = NO_DISPLAYS_META;

  // 屏幕坐标 ↔ bitmap 坐标（1:1）
  const screen1 = { x: 100, y: 200 };
  const bitmap1 = screenToBitmap(screen1.x, screen1.y, meta);
  assertEqual(bitmap1, { x: 100, y: 200 }, 'screen → bitmap');
  const screen1Back = bitmapToScreen(bitmap1.x, bitmap1.y, meta);
  assertEqual(screen1Back, screen1, 'bitmap → screen 往返');

  // bitmap ↔ CSS（fallback dpr=1 → 1:1）
  const css1 = bitmapToCss(bitmap1.x, bitmap1.y, meta);
  assertFloatEqual(css1.x, 100, 'bitmap → CSS x');
  assertFloatEqual(css1.y, 200, 'bitmap → CSS y');
  const bitmap1Back = cssToBitmap(css1.x, css1.y, meta);
  assertEqual(bitmap1Back, bitmap1, 'CSS → bitmap 往返');

  // 矩形转换
  const rectScreen = { x: 50, y: 60, w: 200, h: 100 };
  const rectBitmap = rectScreenToBitmap(rectScreen, meta);
  assertEqual(rectBitmap, rectScreen, 'rect screen → bitmap');
  const rectScreenBack = rectBitmapToScreen(rectBitmap, meta);
  assertEqual(rectScreenBack, rectScreen, 'rect bitmap → screen 往返');

  const rectCss = rectScreenToCss(rectScreen, meta);
  assertEqual(rectCss, { x: 50, y: 60, w: 200, h: 100 }, 'rect screen → CSS');
  const rectScreenBack2 = rectCssToScreen(rectCss, meta);
  assertEqual(rectScreenBack2, rectScreen, 'rect CSS → screen 往返');
}

function testLeftNegativeSecondary() {
  console.log('\n=== 左侧负坐标副屏 ===');
  const meta = { vx: -1920, vy: 0, displays: [] }; // 无 displays → fallback dpr=1

  // 副屏左上角 (0, 0) → 虚拟屏幕 (-1920, 0)
  const css1 = { x: 0, y: 0 };
  const screen1 = cssToScreen(css1.x, css1.y, meta);
  assertEqual(screen1, { x: -1920, y: 0 }, '副屏 CSS (0,0) → screen');
  const bitmap1 = screenToBitmap(screen1.x, screen1.y, meta);
  assertEqual(bitmap1, { x: 0, y: 0 }, '副屏 screen → bitmap');

  // 反向转换
  const screen1Back = bitmapToScreen(bitmap1.x, bitmap1.y, meta);
  assertEqual(screen1Back, screen1, '副屏 bitmap → screen 往返');
  const css1Back = screenToCss(screen1.x, screen1.y, meta);
  assertEqual(css1Back, css1, '副屏 screen → CSS 往返');
}

function testTopNegativeSecondary() {
  console.log('\n=== 上方负坐标副屏 ===');
  const meta = { vx: 0, vy: -1080, displays: [] };

  // 副屏左上角 (0, 0) → 虚拟屏幕 (0, -1080)
  const css1 = { x: 0, y: 0 };
  const screen1 = cssToScreen(css1.x, css1.y, meta);
  assertEqual(screen1, { x: 0, y: -1080 }, '上方副屏 CSS (0,0) → screen');
  const bitmap1 = screenToBitmap(screen1.x, screen1.y, meta);
  assertEqual(bitmap1, { x: 0, y: 0 }, '上方副屏 screen → bitmap');
}

function testWindowPartialOutOfBounds() {
  console.log('\n=== 窗口部分越界 ===');
  const meta = { vx: 0, vy: 0, displays: [] };
  const bitmapWidth = 1920;
  const bitmapHeight = 1080;

  // 窗口右边超出 bitmap
  const rectScreen = { x: 1800, y: 500, w: 200, h: 100 };
  const rectBitmap = rectScreenToBitmap(rectScreen, meta);
  const clamped = clampRectToBitmap(rectBitmap, bitmapWidth, bitmapHeight);
  assertEqual(clamped, { x: 1800, y: 500, w: 120, h: 100 }, '右边越界 clamp');

  // 窗口左边超出 bitmap
  const rectScreen2 = { x: -50, y: 200, w: 100, h: 50 };
  const rectBitmap2 = rectScreenToBitmap(rectScreen2, meta);
  const clamped2 = clampRectToBitmap(rectBitmap2, bitmapWidth, bitmapHeight);
  assertEqual(clamped2, { x: 0, y: 200, w: 50, h: 50 }, '左边越界 clamp');

  // 完全在范围外——x/y 被 clamp 到 bitmap 边界，w/h 归零
  const rectScreen3 = { x: 2500, y: 1200, w: 100, h: 50 };
  const rectBitmap3 = rectScreenToBitmap(rectScreen3, meta);
  const clamped3 = clampRectToBitmap(rectBitmap3, bitmapWidth, bitmapHeight);
  assertEqual(clamped3, { x: 1920, y: 1080, w: 0, h: 0 }, '完全越界 → clamp 到边界 w=0 h=0');
}

function testClampRectToCss() {
  console.log('\n=== CSS clamp ===');
  const overlayWidth = 1920;
  const overlayHeight = 1080;

  // 右边超出
  const rect1 = { x: 1800, y: 500, w: 200, h: 100 };
  const clamped1 = clampRectToCss(rect1, overlayWidth, overlayHeight);
  assertEqual(clamped1, { x: 1800, y: 500, w: 120, h: 100 }, 'CSS 右边越界 clamp');

  // 左边超出
  const rect2 = { x: -50, y: 200, w: 100, h: 50 };
  const clamped2 = clampRectToCss(rect2, overlayWidth, overlayHeight);
  assertEqual(clamped2, { x: 0, y: 200, w: 50, h: 50 }, 'CSS 左边越界 clamp');

  // 完全越界
  const rect3 = { x: 2500, y: 1200, w: 100, h: 50 };
  const clamped3 = clampRectToCss(rect3, overlayWidth, overlayHeight);
  assertEqual(clamped3, { x: 1920, y: 1080, w: 0, h: 0 }, 'CSS 完全越界 → clamp 到边界');
}

function testCssBitmapRoundTrip() {
  console.log('\n=== CSS ↔ bitmap 往返换算 ===');
  const meta = { vx: -500, vy: -300, displays: [] }; // fallback dpr=1

  // 用整数 CSS 坐标避免两次取整的累积误差
  const css1 = { x: 100, y: 200 };
  const bitmap1 = cssToBitmap(css1.x, css1.y, meta);
  const css1Back = bitmapToCss(bitmap1.x, bitmap1.y, meta);
  // 整数 CSS 坐标往返应精确
  assertFloatEqual(css1Back.x, css1.x, 'CSS → bitmap → CSS x 往返');
  assertFloatEqual(css1Back.y, css1.y, 'CSS → bitmap → CSS y 往返');
}

function testPointInRect() {
  console.log('\n=== 点是否在矩形内 ===');
  const rect = { x: 100, y: 100, w: 200, h: 150 };

  assertTrue(pointInRect(150, 120, rect), '点在矩形内部');
  assertFalse(pointInRect(99, 120, rect), '点在左边外');
  assertFalse(pointInRect(301, 120, rect), '点在右边外（排他边界）');
  assertFalse(pointInRect(150, 99, rect), '点在上边外');
  assertFalse(pointInRect(150, 250, rect), '点在下边外（排他边界）');
  assertTrue(pointInRect(100, 100, rect), '点在左上角（包含边界）');
  assertFalse(pointInRect(300, 250, rect), '点在右下角外（排他边界）');
}

function testDistanceCss() {
  console.log('\n=== CSS 距离计算 ===');
  const dist1 = distanceCss(0, 0, 3, 4);
  assertFloatEqual(dist1, 5, '距离 (0,0) → (3,4) = 5');

  const dist2 = distanceCss(100, 100, 103, 104);
  assertFloatEqual(dist2, 5, '距离 (100,100) → (103,104) = 5');
}

function testFormatSelectionInfo() {
  console.log('\n=== 统一格式化 ===');
  const info1 = formatSelectionInfo(100, 200, 300, 150);
  assertTrue(info1 === '(100, 200) 300 × 150 px', `格式化正确: ${info1}`);

  const info2 = formatSelectionInfo(-50, -30, 200, 100);
  assertTrue(info2 === '(-50, -30) 200 × 100 px', `负坐标格式化: ${info2}`);
}

function testFormatColor() {
  console.log('\n=== 色值格式化 ===');

  // HEX 格式
  const hex1 = formatColor(255, 0, 128, 0);
  assertTrue(hex1 === '#FF0080', `HEX 格式: ${hex1}`);

  // RGB 格式
  const rgb1 = formatColor(255, 0, 128, 1);
  assertTrue(rgb1 === 'RGB(255, 0, 128)', `RGB 格式: ${rgb1}`);

  // HSL 格式
  const hsl1 = formatColor(255, 0, 0, 2);
  assertTrue(hsl1 === 'HSL(0, 100%, 50%)', `HSL 格式 (红色): ${hsl1}`);

  const hsl2 = formatColor(0, 255, 0, 2);
  assertTrue(hsl2 === 'HSL(120, 100%, 50%)', `HSL 格式 (绿色): ${hsl2}`);

  const hsl3 = formatColor(0, 0, 255, 2);
  assertTrue(hsl3 === 'HSL(240, 100%, 50%)', `HSL 格式 (蓝色): ${hsl3}`);
}

function testRgbToHsl() {
  console.log('\n=== RGB → HSL 转换 ===');

  // 红色
  const hsl1 = rgbToHsl(255, 0, 0);
  assertEqual(hsl1, [0, 100, 50], '红色 → HSL(0, 100%, 50%)');

  // 绿色
  const hsl2 = rgbToHsl(0, 255, 0);
  assertEqual(hsl2, [120, 100, 50], '绿色 → HSL(120, 100%, 50%)');

  // 蓝色
  const hsl3 = rgbToHsl(0, 0, 255);
  assertEqual(hsl3, [240, 100, 50], '蓝色 → HSL(240, 100%, 50%)');

  // 黑色
  const hsl4 = rgbToHsl(0, 0, 0);
  assertEqual(hsl4, [0, 0, 0], '黑色 → HSL(0, 0%, 0%)');

  // 白色
  const hsl5 = rgbToHsl(255, 255, 255);
  assertEqual(hsl5, [0, 0, 100], '白色 → HSL(0, 0%, 100%)');

  // 灰色
  const hsl6 = rgbToHsl(128, 128, 128);
  assertEqual(hsl6, [0, 0, 50], '灰色 → HSL(0, 0%, 50%)');

  // 橙色
  const hsl7 = rgbToHsl(255, 165, 0);
  assertEqual(hsl7, [39, 100, 50], '橙色 → HSL(39, 100%, 50%)');
}

// ── 主测试入口 ─────────────────────────────────────────────────────────────

function runAllTests() {
  console.log('\n🧪 开始 ss-selection-geometry 测试套件...\n');

  try {
    // 0.18.8 per-monitor 新增测试
    testDprAtScreen();
    testDprAtCss();
    testDprAtCssBoundaryDeterminism();
    testScreenToCssPerMonitor();
    testSameDpiZeroRegression();
    testCssSizeToPhysical();

    // 原有测试（签名已更新）
    testSingleScreen100();
    testLeftNegativeSecondary();
    testTopNegativeSecondary();
    testWindowPartialOutOfBounds();
    testClampRectToCss();
    testCssBitmapRoundTrip();
    testPointInRect();
    testDistanceCss();
    testFormatSelectionInfo();
    testFormatColor();
    testRgbToHsl();

    console.log('\n✅ 所有测试通过！');
    return true;
  } catch (e) {
    console.error('\n❌ 测试失败:', e.message);
    return false;
  }
}

// 允许直接运行测试文件（Node.js 环境）
if (typeof process !== 'undefined' && process.versions?.node) {
  runAllTests();
}

export { runAllTests };
