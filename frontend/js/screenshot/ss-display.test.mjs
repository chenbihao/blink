//! 显示器几何与工具栏放置辅助测试（0.18.7 多屏工具栏出屏修复）。
//!
//! 覆盖：
//! - displayToCss 恒等（后端已注入 overlay CSS 坐标，前端不再折算）
//! - findDisplayContainingRect：矩形完全落在某可见屏内才命中；
//!   跨屏边界 / 空白区 / viewport 外均返回 null
//! - getDisplays / findDisplayCssAt：注入缺失降级、点命中语义

// ── mock 浏览器环境 ───────────────────────────────────────────────────────
// ss-display.js 顶部 import { ss } from './ss-state.js'；ss-state 不依赖 DOM 即可加载。
// 这里只 mock positionToolbar 之外的纯函数所需的最小 window 形态。

function setMeta(meta) {
  globalThis.window = {
    __blinkScreenMeta: meta,
    innerWidth: 3000,
    innerHeight: 2000,
  };
}

const { displayToCss, getDisplays, findDisplayCssAt, findDisplayContainingRect } =
  await import('./ss-display.js');

// ── 断言辅助 ──────────────────────────────────────────────────────────────
function assertEqual(actual, expected, msg) {
  const a = JSON.stringify(actual);
  const e = JSON.stringify(expected);
  if (a !== e) throw new Error(`${msg}: expected ${e}, got ${a}`);
  console.log(`✓ ${msg}`);
}
function assertNull(actual, msg) {
  if (actual !== null) throw new Error(`${msg}: expected null, got ${JSON.stringify(actual)}`);
  console.log(`✓ ${msg}`);
}

// ── 测试用例 ──────────────────────────────────────────────────────────────

function testDisplayToCssIdentity() {
  console.log('\n=== displayToCss 恒等（后端注入 CSS 坐标）===');
  setMeta({ displays: [{ x: 0, y: 0, w: 1920, h: 1080, primary: true }] });
  // 注入值应原样返回，不再除 dpr
  assertEqual(displayToCss({ x: 0, y: 0, w: 1920, h: 1080 }), { x: 0, y: 0, w: 1920, h: 1080 }, '主屏恒等');
  assertEqual(
    displayToCss({ x: 1920, y: -109, w: 2048, h: 1365 }),
    { x: 1920, y: -109, w: 2048, h: 1365 },
    '副屏（负 y）恒等',
  );
}

function testGetDisplaysFallback() {
  console.log('\n=== getDisplays 降级（无注入）===');
  setMeta({});
  assertEqual(getDisplays(), [], '空 displays 返回 []');
  setMeta(undefined);
  globalThis.window = {}; // 完全无 meta
  assertEqual(getDisplays(), [], '无 __blinkScreenMeta 返回 []');
}

function testFindDisplayCssAtPointHit() {
  console.log('\n=== findDisplayCssAt 点命中 ===');
  // 复现日志环境：主屏 0,0 + 副屏右上垂直错位（virtual_y=-109）
  setMeta({
    displays: [
      { x: 0, y: 0, w: 2560, h: 1440, primary: true },
      { x: 2560, y: -109, w: 2048, h: 1365, primary: false },
    ],
  });
  // 主屏内
  assertEqual(findDisplayCssAt(100, 100), { x: 0, y: 0, w: 2560, h: 1440 }, '主屏点命中');
  // 副屏内（含负 y 区）
  assertEqual(findDisplayCssAt(3000, -50), { x: 2560, y: -109, w: 2048, h: 1365 }, '副屏点命中（负 y）');
  // 屏间空白区（x 在副屏范围、y 在主屏底部以下副屏顶部以上的空白）
  // 既有行为：点不中任何屏 -> 回退整个 viewport（含空白）。这是旧 positionToolbar
  // 出屏的成因之一；新 positionToolbar 改用 findDisplayContainingRect 规避此 fallback。
  assertEqual(
    findDisplayCssAt(3000, 1330),
    { x: 0, y: 0, w: 3000, h: 2000 },
    '空白区点不命中 -> viewport fallback（既有行为）',
  );
}

function testFindDisplayCssAtFallbackViewport() {
  console.log('\n=== findDisplayCssAt 降级（无 displays）===');
  setMeta({});
  globalThis.window = { __blinkScreenMeta: { displays: [] }, innerWidth: 3000, innerHeight: 2000 };
  assertEqual(
    findDisplayCssAt(100, 100),
    { x: 0, y: 0, w: 3000, h: 2000 },
    '无 displays -> 整个 viewport fallback',
  );
}

function testFindDisplayContainingRect() {
  console.log('\n=== findDisplayContainingRect 矩形包含检测 ===');
  setMeta({
    displays: [
      { x: 0, y: 0, w: 2560, h: 1440, primary: true },
      { x: 2560, y: -109, w: 2048, h: 1365, primary: false },
    ],
  });
  // 完全在主屏内
  assertEqual(
    findDisplayContainingRect({ left: 10, top: 10, right: 100, bottom: 50 }),
    { x: 0, y: 0, w: 2560, h: 1440 },
    '主屏内矩形命中',
  );
  // 完全在副屏内
  assertEqual(
    findDisplayContainingRect({ left: 2600, top: -100, right: 2700, bottom: 0 }),
    { x: 2560, y: -109, w: 2048, h: 1365 },
    '副屏内矩形命中（负 y）',
  );
  // 跨主副屏边界（右越界出主屏）
  assertNull(
    findDisplayContainingRect({ left: 2500, top: 10, right: 2600, bottom: 50 }),
    '跨主副屏边界 -> null',
  );
  // 落在屏间空白区
  assertNull(
    findDisplayContainingRect({ left: 2600, top: 1330, right: 2700, bottom: 1380 }),
    '空白区矩形 -> null',
  );
  // 矩形部分越出 viewport（右上）
  assertNull(
    findDisplayContainingRect({ left: 2550, top: -200, right: 2700, bottom: -110 }),
    '越出副屏顶部 -> null',
  );
}

// ── 运行 ──────────────────────────────────────────────────────────────────
function runAllTests() {
  console.log('\n🧪 开始 ss-display 测试套件...\n');
  try {
    testDisplayToCssIdentity();
    testGetDisplaysFallback();
    testFindDisplayCssAtPointHit();
    testFindDisplayCssAtFallbackViewport();
    testFindDisplayContainingRect();
    console.log('\n✅ 所有测试通过！');
    return true;
  } catch (e) {
    console.error('\n❌ 测试失败:', e.message);
    return false;
  }
}

if (typeof window === 'undefined' || !globalThis.window) {
  // node 环境：先给个初始 mock，runAllTests 内会按用例重设
  setMeta({});
}

runAllTests();

export { runAllTests };
