//! 坐标契约测试。
//! 核心原则：CSS↔bitmap/screen 的真实比例由 canvas 实测 renderScale 决定，
//! 不由 overlayDpi 或 window.devicePixelRatio 推测。

if (typeof globalThis.window === 'undefined') {
    globalThis.window = {devicePixelRatio: 1};
}

import {
    bitmapPointToCss,
    bitmapToScreen,
    clampRectToBitmap,
    cssPointToBitmap,
    cssPointToScreen,
    cssRectToBitmap,
    cssRectToScreen,
    cssSizeToPhysical,
    distanceCss,
    dprAtCss,
    dprAtScreen,
    formatColor,
    formatSelectionInfo,
    getRenderScale,
    monitorDprAtCss,
    monitorDprAtScreen,
    overlayDpr,
    pointInRect,
    rectBitmapToScreen,
    rectScreenToBitmap,
    rgbToHsl,
    screenPointToCss,
    screenRectToCss,
    screenToBitmap,
    setRenderScale,
    syncRenderScale,
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

function assertFalse(value, msg) {
    if (value) throw new Error(`${msg}: expected false, got ${value}`);
    console.log(`✓ ${msg}`);
}

// ── 真实日志夹具 ──────────────────────────────────────────────────────────
//
// 真实屏幕物理布局：
//   主屏：(0, 0) 2560×1440，DPI 96
//   副屏：(2560, -109) 3072×1920，DPI 192
//   虚拟桌面：(0, -109) 5632×1920
//
// 关键场景：overlayDpi=96 但实际 canvas 比例为 2（canvas 5632×1920, CSS 2816×960）
// 此时 overlayDpi 推测的 dpr=1.0 与真实 renderScale=2 不一致

// 场景 A：overlayDpi=96，renderScale=2（后端 DPI 与真实 canvas 比例不一致）
const META_DPI_MISMATCH = {
    vx: 0, vy: -109, w: 5632, h: 1920, overlayDpi: 96, fgHwnd: 0,
    renderScaleX: 2, renderScaleY: 2,
    physicalDisplays: [
        {x: 0, y: 0, w: 2560, h: 1440, primary: true, dpi: 96},
        {x: 2560, y: -109, w: 3072, h: 1920, primary: false, dpi: 192},
    ]
};

// 场景 B：overlayDpi=96，renderScale=1（单屏 100%，无 displays）
const META_SINGLE_SCREEN = {vx: 0, vy: 0, w: 1920, h: 1080};

// 场景 C：同 DPI 双屏（双 100%），renderScale=1
const META_SAME_DPI = {
    vx: 0, vy: 0, w: 3840, h: 1080, overlayDpi: 96, fgHwnd: 0,
    renderScaleX: 1, renderScaleY: 1,
    physicalDisplays: [
        {x: 0, y: 0, w: 1920, h: 1080, primary: true, dpi: 96},
        {x: 1920, y: 0, w: 1920, h: 1080, primary: false, dpi: 96},
    ]
};

// ── renderScale 基础测试 ──────────────────────────────────────────────────

function testGetRenderScale() {
    console.log('\n=== getRenderScale ===');

    // 有 renderScale 的 meta
    const {scaleX, scaleY} = getRenderScale(META_DPI_MISMATCH);
    assertFloatEqual(scaleX, 2, 'getRenderScale X = 2');
    assertFloatEqual(scaleY, 2, 'getRenderScale Y = 2');

    // 无 renderScale → fallback window.devicePixelRatio
    const fallback = getRenderScale(META_SINGLE_SCREEN);
    assertFloatEqual(fallback.scaleX, 1, 'getRenderScale fallback X = 1');
    assertFloatEqual(fallback.scaleY, 1, 'getRenderScale fallback Y = 1');
}

function testSetRenderScale() {
    console.log('\n=== setRenderScale ===');
    const meta = {};
    setRenderScale(meta, 1.5, 2.0);
    assertFloatEqual(meta.renderScaleX, 1.5, 'setRenderScale X = 1.5');
    assertFloatEqual(meta.renderScaleY, 2.0, 'setRenderScale Y = 2.0');
}

function testSyncRenderScale() {
    console.log('\n=== syncRenderScale ===');
    // mock canvas
    const canvas = {
        width: 5632, height: 1920,
        getBoundingClientRect: () => ({width: 2816, height: 960, left: 0, top: 0})
    };
    const meta = {};
    const ok = syncRenderScale(canvas, meta);
    assertTrue(ok, 'syncRenderScale 成功');
    assertFloatEqual(meta.renderScaleX, 2, 'syncRenderScale X = 5632/2816 = 2');
    assertFloatEqual(meta.renderScaleY, 2, 'syncRenderScale Y = 1920/960 = 2');
    assertFloatEqual(meta.viewportCssWidth, 2816, 'viewportCssWidth = 2816');
    assertFloatEqual(meta.viewportCssHeight, 960, 'viewportCssHeight = 960');

    // 零尺寸 canvas → 返回 false
    const zeroCanvas = {
        width: 0, height: 0,
        getBoundingClientRect: () => ({width: 0, height: 0})
    };
    const ok2 = syncRenderScale(zeroCanvas, {});
    assertFalse(ok2, 'syncRenderScale 零尺寸 → false');
}

// ── DPI 不一致场景测试（核心回归测试） ─────────────────────────────────────

function testDpiMismatchCssToBitmap() {
    console.log('\n=== DPI 不一致：cssRectToBitmap (overlayDpi=96, renderScale=2) ===');

    // CSS {x:711, y:182, w:351, h:258} → bitmap {x:1422, y:364, w:702, h:516}
    const cssRect = {x: 711, y: 182, w: 351, h: 258};
    const bmp = cssRectToBitmap(cssRect, META_DPI_MISMATCH);
    assertEqual(bmp, {x: 1422, y: 364, w: 702, h: 516}, 'cssRectToBitmap (711,182,351,258) → (1422,364,702,516)');

    // 单点也验证
    const pt = cssPointToBitmap(711, 182, META_DPI_MISMATCH);
    assertEqual(pt, {x: 1422, y: 364}, 'cssPointToBitmap (711,182) → (1422,364)');
}

function testDpiMismatchScreenToCss() {
    console.log('\n=== DPI 不一致：screenPointToCss (overlayDpi=96, renderScale=2) ===');

    // UIA physical rect {x:1422, y:255, w:702, h:516}, virtual_y=-109
    // CSS: x = (1422 - 0) / 2 = 711
    //      y = (255 - (-109)) / 2 = 364 / 2 = 182
    //      w = 702 / 2 = 351
    //      h = 516 / 2 = 258
    const screenRect = {x: 1422, y: 255, w: 702, h: 516};
    const cssRect = screenRectToCss(screenRect, META_DPI_MISMATCH);
    assertFloatEqual(cssRect.x, 711, 'UIA screenRectToCss x = 711');
    assertFloatEqual(cssRect.y, 182, 'UIA screenRectToCss y = 182');
    assertFloatEqual(cssRect.w, 351, 'UIA screenRectToCss w = 351');
    assertFloatEqual(cssRect.h, 258, 'UIA screenRectToCss h = 258');
}

function testDpiMismatchRoundTrip() {
    console.log('\n=== DPI 不一致：往返换算 ===');

    // screen → CSS → screen
    const s1 = {x: 1422, y: 255};
    const css1 = screenPointToCss(s1.x, s1.y, META_DPI_MISMATCH);
    const s1Back = cssPointToScreen(css1.x, css1.y, META_DPI_MISMATCH);
    assertEqual(s1Back, {x: 1422, y: 255}, 'screen → CSS → screen 往返');

    // CSS → bitmap → CSS
    const css2 = {x: 711, y: 182};
    const bmp2 = cssPointToBitmap(css2.x, css2.y, META_DPI_MISMATCH);
    const css2Back = bitmapPointToCss(bmp2.x, bmp2.y, META_DPI_MISMATCH);
    assertFloatEqual(css2Back.x, 711, 'CSS → bitmap → CSS x 往返');
    assertFloatEqual(css2Back.y, 182, 'CSS → bitmap → CSS y 往返');

    // rect 往返
    const rect1 = {x: 1422, y: 255, w: 702, h: 516};
    const cssRect = screenRectToCss(rect1, META_DPI_MISMATCH);
    const rect1Back = cssRectToScreen(cssRect, META_DPI_MISMATCH);
    assertEqual(rect1Back, {x: 1422, y: 255, w: 702, h: 516}, 'screen rect → CSS → screen 往返');
}

function testDpiMismatchMonitorDpr() {
    console.log('\n=== DPI 不一致：monitorDprAtCss/AtScreen ===');

    // 主屏物理坐标 (1000, 0) → monitorDprAtScreen → dpr 1.0
    assertFloatEqual(monitorDprAtScreen(1000, 0, META_DPI_MISMATCH), 1.0, 'monitorDprAtScreen 主屏 → 1.0');

    // 副屏物理坐标 (2560, -109) → monitorDprAtScreen → dpr 2.0
    assertFloatEqual(monitorDprAtScreen(2560, -109, META_DPI_MISMATCH), 2.0, 'monitorDprAtScreen 副屏 → 2.0');

    // CSS (711, 182) → cssPointToScreen (1422, 255) → 主屏 → dpr 1.0
    assertFloatEqual(monitorDprAtCss(711, 182, META_DPI_MISMATCH), 1.0, 'monitorDprAtCss (711,182) → 主屏 1.0');

    // CSS (1281, 0) → cssPointToScreen (2562, -109) → 副屏 → dpr 2.0
    assertFloatEqual(monitorDprAtCss(1281, 0, META_DPI_MISMATCH), 2.0, 'monitorDprAtCss (1281,0) → 副屏 2.0');

    // 兼容导出
    assertFloatEqual(dprAtCss(711, 182, META_DPI_MISMATCH), 1.0, 'dprAtCss 兼容导出');
    assertFloatEqual(dprAtScreen(1000, 0, META_DPI_MISMATCH), 1.0, 'dprAtScreen 兼容导出');
}

function testDpiMismatchOverlayDprIsDiagnostic() {
    console.log('\n=== DPI 不一致：overlayDpr 仅诊断，不参与换算 ===');

    // overlayDpi=96 → overlayDpr()=1.0，但 renderScale=2
    assertFloatEqual(overlayDpr(META_DPI_MISMATCH), 1.0, 'overlayDpr = 96/96 = 1.0（诊断值）');
    assertFloatEqual(getRenderScale(META_DPI_MISMATCH).scaleX, 2, 'renderScale = 2（实际换算用值）');

    // cssRectToBitmap 用 renderScale=2，不是 overlayDpr=1
    const bmp = cssRectToBitmap({x: 100, y: 100, w: 100, h: 100}, META_DPI_MISMATCH);
    assertEqual(bmp, {x: 200, y: 200, w: 200, h: 200}, 'cssRectToBitmap 用 renderScale=2（非 overlayDpr=1）');
}

// ── scaleX ≠ scaleY 场景 ──────────────────────────────────────────────────

function testNonUniformScale() {
    console.log('\n=== scaleX ≠ scaleY ===');
    const meta = {
        vx: 0, vy: 0, w: 2000, h: 1000,
        renderScaleX: 2, renderScaleY: 1.5,
        physicalDisplays: [{x: 0, y: 0, w: 2000, h: 1000, primary: true, dpi: 96}],
    };

    // CSS (100, 100) → bitmap (200, 150)
    const bmp = cssPointToBitmap(100, 100, meta);
    assertEqual(bmp, {x: 200, y: 150}, 'cssPointToBitmap scaleX=2, scaleY=1.5');

    // CSS rect → bitmap rect
    const bmpRect = cssRectToBitmap({x: 10, y: 10, w: 100, h: 100}, meta);
    assertEqual(bmpRect, {x: 20, y: 15, w: 200, h: 150}, 'cssRectToBitmap 非均匀 scale');

    // screen → CSS 使用各自的 scale
    const css = screenPointToCss(200, 150, meta);
    assertFloatEqual(css.x, 100, 'screenPointToCss x = 200/2 = 100');
    assertFloatEqual(css.y, 100, 'screenPointToCss y = 150/1.5 = 100');

    // 往返
    const screenBack = cssPointToScreen(css.x, css.y, meta);
    assertEqual(screenBack, {x: 200, y: 150}, '非均匀 scale 往返');
}

// ── 快路径与合成路径一致性 ─────────────────────────────────────────────────

function testFastPathCompositePathConsistency() {
    console.log('\n=== 快路径与合成路径 bitmap rect 一致 ===');
    const meta = META_DPI_MISMATCH;
    const selCss = {x: 711, y: 182, w: 351, h: 258};

    // 快路径：cssRectToBitmap
    const fastBmp = cssRectToBitmap(selCss, meta);

    // 合成路径：也应该用 cssRectToBitmap（不再分别调 cssToBitmap + cssSizeToPhysical）
    const compositeBmp = cssRectToBitmap(selCss, meta);

    assertEqual(fastBmp, compositeBmp, '快路径 = 合成路径');
    assertEqual(fastBmp, {x: 1422, y: 364, w: 702, h: 516}, 'bitmap rect 正确');
}

// ── 原有场景测试 ──────────────────────────────────────────────────────────

function testSingleScreen100() {
    console.log('\n=== 单屏 100% (无 displays, fallback dpr) ===');
    const meta = META_SINGLE_SCREEN;

    const screen1 = {x: 100, y: 200};
    const bitmap1 = screenToBitmap(screen1.x, screen1.y, meta);
    assertEqual(bitmap1, {x: 100, y: 200}, 'screen → bitmap');
    const screen1Back = bitmapToScreen(bitmap1.x, bitmap1.y, meta);
    assertEqual(screen1Back, screen1, 'bitmap → screen 往返');

    const css1 = bitmapPointToCss(bitmap1.x, bitmap1.y, meta);
    assertFloatEqual(css1.x, 100, 'bitmap → CSS x');
    assertFloatEqual(css1.y, 200, 'bitmap → CSS y');
    const bitmap1Back = cssPointToBitmap(css1.x, css1.y, meta);
    assertEqual(bitmap1Back, bitmap1, 'CSS → bitmap 往返');

    const rectScreen = {x: 50, y: 60, w: 200, h: 100};
    const rectBitmap = rectScreenToBitmap(rectScreen, meta);
    assertEqual(rectBitmap, rectScreen, 'rect screen → bitmap');
    const rectScreenBack = rectBitmapToScreen(rectBitmap, meta);
    assertEqual(rectScreenBack, rectScreen, 'rect bitmap → screen 往返');
}

function testSameDpiZeroRegression() {
    console.log('\n=== 同 DPI 双屏零回归 ===');
    const meta = META_SAME_DPI;

    assertFloatEqual(overlayDpr(meta), 1.0, 'overlayDpr = 1.0');
    assertFloatEqual(getRenderScale(meta).scaleX, 1, 'renderScale = 1');

    const css1 = screenPointToCss(100, 200, meta);
    assertFloatEqual(css1.x, 100, '同 DPI screenPointToCss x');
    assertFloatEqual(css1.y, 200, '同 DPI screenPointToCss y');

    const screen1 = cssPointToScreen(100, 200, meta);
    assertEqual(screen1, {x: 100, y: 200}, '同 DPI cssPointToScreen');

    const rect = {x: 50, y: 60, w: 200, h: 100};
    const rectCss = screenRectToCss(rect, meta);
    assertFloatEqual(rectCss.x, 50, 'rectScreenToCss x');
    assertFloatEqual(rectCss.w, 200, 'rectScreenToCss w');
}

function testLeftNegativeSecondary() {
    console.log('\n=== 左侧负坐标副屏 ===');
    const meta = {vx: -1920, vy: 0, renderScaleX: 1, renderScaleY: 1};

    const screen1 = cssPointToScreen(0, 0, meta);
    assertEqual(screen1, {x: -1920, y: 0}, 'CSS (0,0) → screen');
    const bitmap1 = screenToBitmap(screen1.x, screen1.y, meta);
    assertEqual(bitmap1, {x: 0, y: 0}, 'screen → bitmap');
    const css1Back = screenPointToCss(screen1.x, screen1.y, meta);
    assertFloatEqual(css1Back.x, 0, 'screen → CSS 往返');
}

function testTopNegativeSecondary() {
    console.log('\n=== 上方负坐标副屏 ===');
    const meta = {vx: 0, vy: -1080, renderScaleX: 1, renderScaleY: 1};

    const screen1 = cssPointToScreen(0, 0, meta);
    assertEqual(screen1, {x: 0, y: -1080}, 'CSS (0,0) → screen');
}

function testWindowPartialOutOfBounds() {
    console.log('\n=== 窗口部分越界 ===');
    const meta = {vx: 0, vy: 0, renderScaleX: 1, renderScaleY: 1};
    const rectScreen = {x: 1800, y: 500, w: 200, h: 100};
    const rectBitmap = rectScreenToBitmap(rectScreen, meta);
    const clamped = clampRectToBitmap(rectBitmap, 1920, 1080);
    assertEqual(clamped, {x: 1800, y: 500, w: 120, h: 100}, '右边越界 clamp');
}

function testCrossDpiClamp() {
    console.log('\n=== 跨 DPI 选区 clamp ===');
    // CSS (711, 182) 在主屏，(1281, 0) 在副屏
    const startDpr = monitorDprAtCss(711, 182, META_DPI_MISMATCH);
    const curDpr = monitorDprAtCss(1281, 0, META_DPI_MISMATCH);
    assertTrue(startDpr !== curDpr, '跨 DPI 边界：起点 dpr !== 当前 dpr');
}

function testCssSizeToPhysicalDeprecated() {
    console.log('\n=== cssSizeToPhysical (deprecated, 兼容) ===');
    // 200 * scaleX(=2) = 400
    assertEqual(cssSizeToPhysical(200, 711, 182, META_DPI_MISMATCH), 400, 'cssSizeToPhysical 200 → 400 (scaleX=2)');
}

function testPointInRect() {
    console.log('\n=== 点是否在矩形内 ===');
    const rect = {x: 100, y: 100, w: 200, h: 150};
    assertTrue(pointInRect(150, 120, rect), '点在矩形内部');
    assertFalse(pointInRect(301, 120, rect), '点在右边外（排他边界）');
}

function testDistanceCss() {
    console.log('\n=== CSS 距离计算 ===');
    assertFloatEqual(distanceCss(0, 0, 3, 4), 5, '距离 (0,0) → (3,4) = 5');
}

function testFormatSelectionInfo() {
    console.log('\n=== 统一格式化 ===');
    const info = formatSelectionInfo(100, 200, 300, 150);
    assertTrue(info === '(100, 200) 300 × 150 px', `格式化正确: ${info}`);
}

function testFormatColor() {
    console.log('\n=== 色值格式化 ===');
    assertTrue(formatColor(255, 0, 128, 0) === '#FF0080', 'HEX 格式');
    assertTrue(formatColor(255, 0, 128, 1) === 'rgb(255, 0, 128)', 'RGB 格式');
}

function testRgbToHsl() {
    console.log('\n=== RGB → HSL 转换 ===');
    assertEqual(rgbToHsl(255, 0, 0), [0, 100, 50], '红色 → HSL');
    assertEqual(rgbToHsl(0, 255, 0), [120, 100, 50], '绿色 → HSL');
    assertEqual(rgbToHsl(0, 0, 255), [240, 100, 50], '蓝色 → HSL');
}

// ── 主测试入口 ─────────────────────────────────────────────────────────────

function runAllTests() {
    console.log('\n🧪 开始 ss-selection-geometry 测试套件...\n');
    try {
        // renderScale 基础
        testGetRenderScale();
        testSetRenderScale();
        testSyncRenderScale();

        // DPI 不一致核心测试
        testDpiMismatchCssToBitmap();
        testDpiMismatchScreenToCss();
        testDpiMismatchRoundTrip();
        testDpiMismatchMonitorDpr();
        testDpiMismatchOverlayDprIsDiagnostic();

        // 非均匀 scale
        testNonUniformScale();

        // 快路径/合成路径一致性
        testFastPathCompositePathConsistency();

        // 原有场景
        testSingleScreen100();
        testSameDpiZeroRegression();
        testLeftNegativeSecondary();
        testTopNegativeSecondary();
        testWindowPartialOutOfBounds();
        testCrossDpiClamp();
        testCssSizeToPhysicalDeprecated();
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

if (typeof process !== 'undefined' && process.versions?.node) {
    const ok = runAllTests();
    if (!ok) process.exit(1);
}

export {runAllTests};
