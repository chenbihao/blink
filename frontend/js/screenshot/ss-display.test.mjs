//! 显示器几何与工具栏放置辅助测试。
//!
//! 覆盖：
//! - getDisplays 从 physicalDisplays 实时转换 CSS
//! - findDisplayCssAt：点命中语义
//! - findDisplayContainingRect：矩形包含检测
//! - getDisplays 降级

function setMeta(meta) {
  globalThis.window = {
    __blinkScreenMeta: meta,
    innerWidth: 3000,
    innerHeight: 2000,
    devicePixelRatio: 1,
  };
}

const { getDisplays, getPhysicalDisplays, findDisplayCssAt, findDisplayContainingRect } =
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
function assertFloatEqual(actual, expected, msg, tol = 0.001) {
  if (Math.abs(actual - expected) > tol) throw new Error(`${msg}: expected ${expected}, got ${actual}`);
  console.log(`✓ ${msg}`);
}

// ── 测试用例 ──────────────────────────────────────────────────────────────

function testGetDisplaysFromPhysical() {
  console.log('\n=== getDisplays 从 physicalDisplays 实时转换 ===');
  // renderScale=2，物理显示器转 CSS
  setMeta({
    renderScaleX: 2, renderScaleY: 2,
    vx: 0, vy: -109,
    physicalDisplays: [
      { x: 0, y: 0, w: 2560, h: 1440, primary: true, dpi: 96 },
      { x: 2560, y: -109, w: 3072, h: 1920, primary: false, dpi: 192 },
    ],
  });
  const displays = getDisplays();
  assertEqual(displays.length, 2, '两块显示器');

  // 主屏：物理 (0,0) 2560×1440 → CSS (0, (0-(-109))/2) = (0, 54.5) 1280×720
  assertFloatEqual(displays[0].x, 0, '主屏 CSS x = 0');
  assertFloatEqual(displays[0].y, 54.5, '主屏 CSS y = 54.5');
  assertFloatEqual(displays[0].w, 1280, '主屏 CSS w = 1280');
  assertFloatEqual(displays[0].h, 720, '主屏 CSS h = 720');
  assertEqual(displays[0].dpi, 96, '主屏 dpi = 96');

  // 副屏：物理 (2560,-109) 3072×1920 → CSS (1280, 0) 1536×960
  assertFloatEqual(displays[1].x, 1280, '副屏 CSS x = 1280');
  assertFloatEqual(displays[1].y, 0, '副屏 CSS y = 0');
  assertFloatEqual(displays[1].w, 1536, '副屏 CSS w = 1536');
  assertFloatEqual(displays[1].h, 960, '副屏 CSS h = 960');
  assertEqual(displays[1].dpi, 192, '副屏 dpi = 192');
}

function testGetPhysicalDisplays() {
  console.log('\n=== getPhysicalDisplays 原始物理矩形 ===');
  setMeta({
    physicalDisplays: [
      { x: 0, y: 0, w: 1920, h: 1080, primary: true, dpi: 96 },
    ],
  });
  const phys = getPhysicalDisplays();
  assertEqual(phys.length, 1, '一块物理显示器');
  assertEqual(phys[0].x, 0, '物理 x = 0');
  assertEqual(phys[0].w, 1920, '物理 w = 1920');
}

function testGetDisplaysFallback() {
  console.log('\n=== getDisplays 降级（无注入）===');
  setMeta({});
  assertEqual(getDisplays(), [], '空 physicalDisplays 返回 []');
  setMeta(undefined);
  globalThis.window = {};
  assertEqual(getDisplays(), [], '无 __blinkScreenMeta 返回 []');
}

function testFindDisplayCssAtPointHit() {
  console.log('\n=== findDisplayCssAt 点命中 ===');
  // renderScale=1，物理坐标即 CSS 坐标（减去 virtual origin）
  setMeta({
    renderScaleX: 1, renderScaleY: 1,
    vx: 0, vy: 0,
    physicalDisplays: [
      { x: 0, y: 0, w: 2560, h: 1440, primary: true, dpi: 96 },
      { x: 2560, y: 0, w: 2048, h: 1365, primary: false, dpi: 192 },
    ],
  });
  // 主屏内
  const d1 = findDisplayCssAt(100, 100);
  assertEqual(d1.x, 0, '主屏点命中 x');
  assertEqual(d1.w, 2560, '主屏点命中 w');
  // 副屏内
  const d2 = findDisplayCssAt(3000, 100);
  assertEqual(d2.x, 2560, '副屏点命中 x');
}

function testFindDisplayContainingRect() {
  console.log('\n=== findDisplayContainingRect 矩形包含检测 ===');
  setMeta({
    renderScaleX: 1, renderScaleY: 1,
    vx: 0, vy: 0,
    physicalDisplays: [
      { x: 0, y: 0, w: 2560, h: 1440, primary: true, dpi: 96 },
      { x: 2560, y: 0, w: 2048, h: 1365, primary: false, dpi: 192 },
    ],
  });
  // 完全在主屏内
  assertEqual(
    findDisplayContainingRect({ left: 10, top: 10, right: 100, bottom: 50 }),
    { x: 0, y: 0, w: 2560, h: 1440, dpi: 96, primary: true },
    '主屏内矩形命中',
  );
  // 完全在副屏内
  assertEqual(
    findDisplayContainingRect({ left: 2600, top: 10, right: 2700, bottom: 50 }),
    { x: 2560, y: 0, w: 2048, h: 1365, dpi: 192, primary: false },
    '副屏内矩形命中',
  );
  // 跨主副屏边界
  assertNull(
    findDisplayContainingRect({ left: 2500, top: 10, right: 2600, bottom: 50 }),
    '跨主副屏边界 -> null',
  );
}

// ── 运行 ──────────────────────────────────────────────────────────────────
function runAllTests() {
  console.log('\n🧪 开始 ss-display 测试套件...\n');
  try {
    testGetDisplaysFromPhysical();
    testGetPhysicalDisplays();
    testGetDisplaysFallback();
    testFindDisplayCssAtPointHit();
    testFindDisplayContainingRect();
    console.log('\n✅ 所有测试通过！');
    return true;
  } catch (e) {
    console.error('\n❌ 测试失败:', e.message);
    return false;
  }
}

if (typeof window === 'undefined' || !globalThis.window) {
  setMeta({});
}

runAllTests();

export { runAllTests };
