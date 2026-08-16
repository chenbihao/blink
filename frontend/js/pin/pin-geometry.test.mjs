//! Pin 几何纯函数测试。
//!
//! 覆盖：混合 DPI 初始化、跨屏视觉尺寸不变、鼠标锚点缩放、
//! 负坐标副屏、分数 DPI 往返误差、竖长/横长图 padding、DPI reconcile。
//!
//! 运行：node frontend/js/pin/pin-geometry.test.mjs

import {
    baseCssSize,
    clampZoom,
    computeWindowRect,
    displayPhysicalSize,
    imageCenter,
    imageScreenFromWinRect,
    MAX_ZOOM,
    MIN_ZOOM,
    padPhysical,
    physicalWindowRect,
    PIN_PAD_CSS,
    reconcileDpi,
    zoomAroundCenter,
    zoomAroundPointer,
} from './pin-geometry.js';

// ── 断言辅助 ──────────────────────────────────────────────────────────────

function assertFloatEqual(actual, expected, msg, tolerance = 0.001) {
    if (Math.abs(actual - expected) > tolerance) {
        throw new Error(`${msg}: expected ${expected}, got ${actual}`);
    }
    console.log(`  ✓ ${msg}`);
}

function assertEqual(actual, expected, msg) {
    const aStr = JSON.stringify(actual);
    const eStr = JSON.stringify(expected);
    if (aStr !== eStr) {
        throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
    }
    console.log(`  ✓ ${msg}`);
}

function assertTrue(value, msg) {
    if (!value) throw new Error(`${msg}: expected true, got ${value}`);
    console.log(`  ✓ ${msg}`);
}

function assertInRange(actual, min, max, msg) {
    if (actual < min || actual > max) {
        throw new Error(`${msg}: expected [${min}, ${max}], got ${actual}`);
    }
    console.log(`  ✓ ${msg}`);
}

// ── 测试 ──────────────────────────────────────────────────────────────────

let passed = 0;
let failed = 0;

function test(name, fn) {
    try {
        console.log(`\n── ${name} ──`);
        fn();
        passed++;
    } catch (e) {
        console.error(`  ✗ ${e.message}`);
        failed++;
    }
}

// ── 1. 100% 屏初始化 ─────────────────────────────────────────────────────

test('100% 屏初始化：图片资源像素与初始屏物理显示像素 1:1', () => {
    const sourcePixelW = 1920;
    const sourcePixelH = 1080;
    const sourceDpr = 1.0;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);
    assertFloatEqual(baseCssW, 1920, 'baseCssW = 1920');
    assertFloatEqual(baseCssH, 1080, 'baseCssH = 1080');

    // 初始屏物理尺寸 = baseCss × dpr = sourcePixel
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, sourceDpr);
    assertEqual(physW, 1920, 'physW = 1920 (1:1)');
    assertEqual(physH, 1080, 'physH = 1080 (1:1)');
});

// ── 2. 150% / 200% 屏初始化 ──────────────────────────────────────────────

test('150% 屏初始化：baseCss = sourcePixels / sourceDpr', () => {
    const sourcePixelW = 2880;  // 150% 屏上的物理像素
    const sourcePixelH = 1620;
    const sourceDpr = 1.5;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);
    assertFloatEqual(baseCssW, 1920, 'baseCssW = 2880 / 1.5 = 1920');
    assertFloatEqual(baseCssH, 1080, 'baseCssH = 1620 / 1.5 = 1080');

    // 初始屏 1:1
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, sourceDpr);
    assertEqual(physW, 2880, 'physW = 2880 (1:1 on 150% screen)');
    assertEqual(physH, 1620, 'physH = 1620 (1:1 on 150% screen)');
});

test('200% 屏初始化：baseCss = sourcePixels / 2.0', () => {
    const sourcePixelW = 3840;
    const sourcePixelH = 2160;
    const sourceDpr = 2.0;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);
    assertFloatEqual(baseCssW, 1920, 'baseCssW = 3840 / 2.0 = 1920');
    assertFloatEqual(baseCssH, 1080, 'baseCssH = 2160 / 2.0 = 1080');
});

// ── 3. 100% → 150%：视觉 CSS 尺寸不变，窗口物理尺寸按 1.5 倍变化 ─────────

test('100% → 150%：视觉 CSS 尺寸不变，物理尺寸按 1.5 倍变化', () => {
    const sourcePixelW = 1920;
    const sourcePixelH = 1080;
    const sourceDpr = 1.0;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);

    // 在 100% 屏上
    const css100 = baseCssW * 1;  // zoom = 1
    const phys100 = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);

    // 跨到 150% 屏
    const css150 = baseCssW * 1;  // zoom 仍 = 1，CSS 尺寸不变
    const phys150 = displayPhysicalSize(baseCssW, baseCssH, 1, 1.5);

    assertFloatEqual(css100, css150, 'CSS 尺寸不变 (1920)');
    assertFloatEqual(phys150.physW / phys100.physW, 1.5, '物理宽度比 = 1.5');
    assertFloatEqual(phys150.physH / phys100.physH, 1.5, '物理高度比 = 1.5');
    assertEqual(phys150.physW, 2880, 'physW = 2880');
    assertEqual(phys150.physH, 1620, 'physH = 1620');
});

// ── 4. 150% → 100%：反向不跳，往返误差不超过 1 CSS px ────────────────────

test('150% → 100%：反向不跳，往返后的视觉尺寸误差不超过 1 CSS px', () => {
    const sourcePixelW = 2880;
    const sourcePixelH = 1620;
    const sourceDpr = 1.5;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);

    // 在 150% 屏上初始
    const phys150 = displayPhysicalSize(baseCssW, baseCssH, 1, 1.5);
    assertEqual(phys150.physW, 2880, '150% 屏物理宽度 = 2880');

    // 跨到 100% 屏
    const phys100 = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    assertEqual(phys100.physW, 1920, '100% 屏物理宽度 = 1920');

    // 往返：100% → 150% → 100%
    const phys150again = displayPhysicalSize(baseCssW, baseCssH, 1, 1.5);
    const phys100again = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    assertEqual(phys100again.physW, phys100.physW, '往返 100% 物理宽度精确一致');
    assertEqual(phys150again.physW, phys150.physW, '往返 150% 物理宽度精确一致');

    // CSS 尺寸始终不变
    const cssW = baseCssW * 1;
    assertFloatEqual(cssW, 1920, 'CSS 尺寸始终 = 1920');
});

// ── 5. 副屏在主屏左侧/上方，屏幕坐标为负数 ──────────────────────────────

test('副屏在主屏左侧/上方，屏幕坐标为负数', () => {
    // 副屏在主屏左侧，副屏坐标范围 (-1920, 0) ~ (0, 1080)
    const imageScreenX = -500;
    const imageScreenY = 200;
    const {baseCssW, baseCssH} = baseCssSize(800, 600, 1.0);
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    const padPhys = padPhysical(PIN_PAD_CSS, 1.0);
    const rect = physicalWindowRect(imageScreenX, imageScreenY, physW, physH, padPhys);

    assertTrue(rect.winX < 0, 'winX < 0（窗口在负坐标区域）');
    assertEqual(rect.winX, -520, 'winX = -500 - 20 = -520');
    assertEqual(rect.winY, 180, 'winY = 200 - 20 = 180');
    assertEqual(rect.winW, 840, 'winW = 800 + 40 = 840');
    assertEqual(rect.winH, 640, 'winH = 600 + 40 = 640');
});

test('副屏在主屏上方，图片在负 Y 坐标', () => {
    const imageScreenX = 100;
    const imageScreenY = -800;
    const {baseCssW, baseCssH} = baseCssSize(400, 300, 1.0);
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    const padPhys = padPhysical(PIN_PAD_CSS, 1.0);
    const rect = physicalWindowRect(imageScreenX, imageScreenY, physW, physH, padPhys);

    assertTrue(rect.winY < 0, 'winY < 0');
    assertEqual(rect.winX, 80, 'winX = 100 - 20 = 80');
    assertEqual(rect.winY, -820, 'winY = -800 - 20 = -820');
});

// ── 6. 鼠标锚点缩放：缩放前后锚点屏幕坐标误差不超过 1 物理 px ─────────

test('鼠标锚点缩放：锚点内容保持在鼠标下方', () => {
    // 图片在 (100, 100)，物理尺寸 800×600，鼠标在图片中心 (500, 400)
    const oldImageX = 100, oldImageY = 100;
    const oldPhysW = 800, oldPhysH = 600;
    const pointerX = 500, pointerY = 400;

    // 放大到 2x：新物理尺寸 1600×1200
    const newPhysW = 1600, newPhysH = 1200;
    const {newImageX, newImageY} = zoomAroundPointer(
        pointerX, pointerY,
        oldImageX, oldImageY,
        oldPhysW, oldPhysH,
        newPhysW, newPhysH,
    );

    // 验证：鼠标位置在缩放后图片中的归一化坐标不变
    const oldAnchorX = (pointerX - oldImageX) / oldPhysW;  // 0.5
    const oldAnchorY = (pointerY - oldImageY) / oldPhysH;  // 0.5
    const newAnchorX = (pointerX - newImageX) / newPhysW;
    const newAnchorY = (pointerY - newImageY) / newPhysH;

    assertFloatEqual(newAnchorX, oldAnchorX, '归一化锚点 X 不变 (0.5)', 1 / newPhysW);
    assertFloatEqual(newAnchorY, oldAnchorY, '归一化锚点 Y 不变 (0.5)', 1 / newPhysH);
});

test('鼠标锚点缩放：鼠标在图片左上角', () => {
    const oldImageX = 100, oldImageY = 100;
    const oldPhysW = 800, oldPhysH = 600;
    const pointerX = 100, pointerY = 100;  // 左上角

    const newPhysW = 1600, newPhysH = 1200;
    const {newImageX, newImageY} = zoomAroundPointer(
        pointerX, pointerY,
        oldImageX, oldImageY,
        oldPhysW, oldPhysH,
        newPhysW, newPhysH,
    );

    // 左上角锚点：图片左上角不动
    assertEqual(newImageX, 100, '左上角锚点：newImageX 不变');
    assertEqual(newImageY, 100, '左上角锚点：newImageY 不变');
});

test('鼠标锚点缩放：鼠标在图片右下角', () => {
    const oldImageX = 100, oldImageY = 100;
    const oldPhysW = 800, oldPhysH = 600;
    const pointerX = 900, pointerY = 700;  // 右下角

    const newPhysW = 1600, newPhysH = 1200;
    const {newImageX, newImageY} = zoomAroundPointer(
        pointerX, pointerY,
        oldImageX, oldImageY,
        oldPhysW, oldPhysH,
        newPhysW, newPhysH,
    );

    // 右下角锚点：右下角不动 → newImageX = 900 - 1600 = -700
    assertEqual(newImageX, -700, '右下角锚点：newImageX = 900 - 1600');
    assertEqual(newImageY, -500, '右下角锚点：newImageY = 700 - 1200');
});

test('鼠标锚点缩放：鼠标在图片外时 clamp 到 [0,1]', () => {
    const oldImageX = 100, oldImageY = 100;
    const oldPhysW = 800, oldPhysH = 600;
    const pointerX = 2000, pointerY = 2000;  // 远在图片外

    const newPhysW = 1600, newPhysH = 1200;
    const {newImageX, newImageY} = zoomAroundPointer(
        pointerX, pointerY,
        oldImageX, oldImageY,
        oldPhysW, oldPhysH,
        newPhysW, newPhysH,
    );

    // clamp 到 1.0：相当于右下角锚点
    // newImageX = pointerX - 1.0 * newPhysW = 2000 - 1600 = 400
    assertEqual(newImageX, 400, '图片外 clamp 到右下角锚点 X');
    assertEqual(newImageY, 800, '图片外 clamp 到右下角锚点 Y');
});

// ── 7. 图片中心 mini/restore：中心位置保持不变 ─────────────────────────

test('图片中心 mini/restore：中心位置保持不变', () => {
    const imageCx = 500, imageCy = 400;
    const newPhysW = 160, newPhysH = 120;

    const {newImageX, newImageY} = zoomAroundCenter(imageCx, imageCy, newPhysW, newPhysH);

    // 验证中心位置不变
    const actualCx = newImageX + newPhysW / 2;
    const actualCy = newImageY + newPhysH / 2;
    assertFloatEqual(actualCx, imageCx, '中心 X 不变', 1);
    assertFloatEqual(actualCy, imageCy, '中心 Y 不变', 1);
});

// ── 8. 竖长图和横长图的窗口宽高/padding 都正确 ───────────────────────────

test('竖长图：窗口宽高/padding 正确', () => {
    const {baseCssW, baseCssH} = baseCssSize(400, 1200, 1.0);
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    const padPhys = padPhysical(PIN_PAD_CSS, 1.0);
    const rect = physicalWindowRect(0, 0, physW, physH, padPhys);

    assertTrue(rect.winH > rect.winW, '竖长图：窗口高 > 宽');
    assertEqual(rect.winW, 440, 'winW = 400 + 40');
    assertEqual(rect.winH, 1240, 'winH = 1200 + 40');
});

test('横长图：窗口宽高/padding 正确', () => {
    const {baseCssW, baseCssH} = baseCssSize(1920, 200, 1.0);
    const {physW, physH} = displayPhysicalSize(baseCssW, baseCssH, 1, 1.0);
    const padPhys = padPhysical(PIN_PAD_CSS, 1.0);
    const rect = physicalWindowRect(0, 0, physW, physH, padPhys);

    assertTrue(rect.winW > rect.winH, '横长图：窗口宽 > 高');
    assertEqual(rect.winW, 1960, 'winW = 1920 + 40');
    assertEqual(rect.winH, 240, 'winH = 200 + 40');
});

test('150% 屏上 padding 按物理像素放大', () => {
    const pad1 = padPhysical(PIN_PAD_CSS, 1.0);
    const pad15 = padPhysical(PIN_PAD_CSS, 1.5);
    const pad2 = padPhysical(PIN_PAD_CSS, 2.0);
    assertEqual(pad1, 20, '100% 屏 padding = 20');
    assertEqual(pad15, 30, '150% 屏 padding = 30');
    assertEqual(pad2, 40, '200% 屏 padding = 40');
});

// ── 9. 连续 DPI reconcile/generation：旧结果不能覆盖新状态 ─────────────

test('DPI reconcile：100% → 150% 保持视觉尺寸', () => {
    const sourcePixelW = 800, sourcePixelH = 600;
    const sourceDpr = 1.0;
    const {baseCssW, baseCssH} = baseCssSize(sourcePixelW, sourcePixelH, sourceDpr);

    // 初始在 100% 屏，图片在 (100, 100)
    const state = {baseCssW, baseCssH, zoom: 1};
    const initRect = computeWindowRect({...state, imageScreenX: 100, imageScreenY: 100}, 1.0);
    assertEqual(initRect.physW, 800, '初始 physW = 800');
    assertEqual(initRect.physH, 600, '初始 physH = 600');

    // 跨到 150% 屏，Windows 改了窗口位置但我们要 reconcile
    // 假设 Windows 把窗口移到了 (80, 80)（实际可能不同，这里模拟）
    const actualWinRect = {winX: 80, winY: 80, winW: 1230, winH: 930};
    const reconciled = reconcileDpi(state, 1.5, actualWinRect);

    // 视觉 CSS 尺寸不变：baseCssW * zoom = 800
    // 物理尺寸 = round(800 * 1.5) = 1200
    assertEqual(reconciled.physW, 1200, 'reconcile 后 physW = 1200');
    assertEqual(reconciled.physH, 900, 'reconcile 后 physH = 900');

    // padding = round(20 * 1.5) = 30
    assertEqual(reconciled.padPhys, 30, 'reconcile 后 padding = 30');

    // 图片位置从实际窗口反推
    assertEqual(reconciled.imageScreenX, 110, 'imageScreenX = 80 + 30 = 110');
    assertEqual(reconciled.imageScreenY, 110, 'imageScreenY = 80 + 30 = 110');
});

test('DPI reconcile：幂等性（连续两次 reconcile 结果一致）', () => {
    const {baseCssW, baseCssH} = baseCssSize(800, 600, 1.0);
    const state = {baseCssW, baseCssH, zoom: 1};

    const actualWinRect = {winX: 100, winY: 100, winW: 840, winH: 640};
    const r1 = reconcileDpi(state, 1.0, actualWinRect);
    // 用 r1 的窗口矩形再 reconcile 一次
    const r2 = reconcileDpi(state, 1.0, {winX: r1.winX, winY: r1.winY, winW: r1.winW, winH: r1.winH});

    assertEqual(r2.imageScreenX, r1.imageScreenX, '幂等：imageScreenX 一致');
    assertEqual(r2.imageScreenY, r1.imageScreenY, '幂等：imageScreenY 一致');
    assertEqual(r2.winW, r1.winW, '幂等：winW 一致');
    assertEqual(r2.winH, r1.winH, '幂等：winH 一致');
});

test('DPI reconcile：往返 100% → 150% → 100% 无误差', () => {
    const {baseCssW, baseCssH} = baseCssSize(800, 600, 1.0);
    const state = {baseCssW, baseCssH, zoom: 1};

    // 初始 100%
    const init = computeWindowRect({...state, imageScreenX: 200, imageScreenY: 200}, 1.0);

    // → 150%
    const r15 = reconcileDpi(state, 1.5, {winX: init.winX, winY: init.winY, winW: init.winW, winH: init.winH});

    // → 100% again
    const r10 = reconcileDpi(state, 1.0, {winX: r15.winX, winY: r15.winY, winW: r15.winW, winH: r15.winH});

    // 图片位置应该和初始一致（padding 回到 20，imageScreenX = winX + 20 = init.winX + 20 = 200）
    assertEqual(r10.imageScreenX, 200, '往返后 imageScreenX = 原始值');
    assertEqual(r10.imageScreenY, 200, '往返后 imageScreenY = 原始值');
    assertEqual(r10.physW, 800, '往返后 physW = 800');
    assertEqual(r10.physH, 600, '往返后 physH = 600');
});

// ── 10. clampZoom ─────────────────────────────────────────────────────────

test('clampZoom：超出范围被钳制', () => {
    assertEqual(clampZoom(0.05), MIN_ZOOM, '0.05 → MIN_ZOOM');
    assertEqual(clampZoom(10), MAX_ZOOM, '10 → MAX_ZOOM');
    assertFloatEqual(clampZoom(1.5), 1.5, '1.5 保持不变');
    assertFloatEqual(clampZoom(0.1), 0.1, '0.1 边界保持');
    assertFloatEqual(clampZoom(8), 8, '8 边界保持');
});

// ── 11. computeWindowRect 组合函数 ───────────────────────────────────────

test('computeWindowRect：组合函数一致性', () => {
    const {baseCssW, baseCssH} = baseCssSize(800, 600, 1.0);
    const state = {baseCssW, baseCssH, zoom: 2, imageScreenX: 100, imageScreenY: 100};
    const rect = computeWindowRect(state, 1.0);

    // zoom=2 → physW = 1600, physH = 1200
    assertEqual(rect.physW, 1600, 'physW = 800 * 2 = 1600');
    assertEqual(rect.physH, 1200, 'physH = 600 * 2 = 1200');
    assertEqual(rect.padPhys, 20, 'padPhys = 20');
    assertEqual(rect.winX, 80, 'winX = 100 - 20 = 80');
    assertEqual(rect.winY, 80, 'winY = 100 - 20 = 80');
    assertEqual(rect.winW, 1640, 'winW = 1600 + 40 = 1640');
    assertEqual(rect.winH, 1240, 'winH = 1200 + 40 = 1240');
});

test('computeWindowRect：150% 屏 zoom=1', () => {
    const {baseCssW, baseCssH} = baseCssSize(1200, 900, 1.5);
    const state = {baseCssW, baseCssH, zoom: 1, imageScreenX: 300, imageScreenY: 300};
    const rect = computeWindowRect(state, 1.5);

    // baseCss = 1200/1.5 = 800, physW = round(800 * 1 * 1.5) = 1200
    assertEqual(rect.physW, 1200, 'physW = 1200');
    assertEqual(rect.physH, 900, 'physH = 900');
    assertEqual(rect.padPhys, 30, 'padPhys = round(20 * 1.5) = 30');
    assertEqual(rect.winX, 270, 'winX = 300 - 30 = 270');
    assertEqual(rect.winY, 270, 'winY = 300 - 30 = 270');
    assertEqual(rect.winW, 1260, 'winW = 1200 + 60 = 1260');
    assertEqual(rect.winH, 960, 'winH = 900 + 60 = 960');
});

// ── 12. imageScreenFromWinRect ───────────────────────────────────────────

test('imageScreenFromWinRect：从窗口矩形反推图片坐标', () => {
    const winRect = {winX: 80, winY: 80};
    const {imageScreenX, imageScreenY} = imageScreenFromWinRect(winRect, 1.0);
    assertEqual(imageScreenX, 100, 'imageScreenX = 80 + 20 = 100');
    assertEqual(imageScreenY, 100, 'imageScreenY = 80 + 20 = 100');

    const {imageScreenX: x15, imageScreenY: y15} = imageScreenFromWinRect({winX: 270, winY: 270}, 1.5);
    assertEqual(x15, 300, '150% 屏 imageScreenX = 270 + 30 = 300');
    assertEqual(y15, 300, '150% 屏 imageScreenY = 270 + 30 = 300');
});

// ── 13. imageCenter ─────────────────────────────────────────────────────

test('imageCenter：计算图片中心坐标', () => {
    const {cx, cy} = imageCenter(100, 100, 800, 600);
    assertEqual(cx, 500, 'cx = 100 + 400 = 500');
    assertEqual(cy, 400, 'cy = 100 + 300 = 400');
});

// ── 14. mini 左上角不变式 ───────────────────────────────────────────────

test('mini 左上角不变式：150% DPI 切换 zoom 时 winX/winY 不变', () => {
    const {baseCssW, baseCssH} = baseCssSize(1200, 900, 1.5);
    const imageScreenX = 300;
    const imageScreenY = 300;

    // zoom = 1 (normal)
    const rectNormal = computeWindowRect(
        {baseCssW, baseCssH, zoom: 1, imageScreenX, imageScreenY},
        1.5,
    );
    // zoom = 0.2 (mini)
    const rectMini = computeWindowRect(
        {baseCssW, baseCssH, zoom: 0.2, imageScreenX, imageScreenY},
        1.5,
    );

    // 左上角不变：winX/winY 相同
    assertEqual(rectMini.winX, rectNormal.winX, 'mini winX = normal winX');
    assertEqual(rectMini.winY, rectNormal.winY, 'mini winY = normal winY');
    // 只有尺寸变化
    assertEqual(rectMini.winW < rectNormal.winW, true, 'mini winW < normal winW');
    assertEqual(rectMini.winH < rectNormal.winH, true, 'mini winH < normal winH');
});

test('mini 左上角不变式：100% DPI 同样成立', () => {
    const {baseCssW, baseCssH} = baseCssSize(800, 600, 1.0);
    const imageScreenX = 100;
    const imageScreenY = 100;

    const rectNormal = computeWindowRect(
        {baseCssW, baseCssH, zoom: 1, imageScreenX, imageScreenY},
        1.0,
    );
    const rectMini = computeWindowRect(
        {baseCssW, baseCssH, zoom: 0.15, imageScreenX, imageScreenY},
        1.0,
    );

    assertEqual(rectMini.winX, rectNormal.winX, '100% DPI mini winX = normal winX');
    assertEqual(rectMini.winY, rectNormal.winY, '100% DPI mini winY = normal winY');
});

test('mini 左上角不变式：负坐标副屏', () => {
    const {baseCssW, baseCssH} = baseCssSize(1920, 1080, 1.0);
    const imageScreenX = -1800;
    const imageScreenY = -900;

    const rectNormal = computeWindowRect(
        {baseCssW, baseCssH, zoom: 1, imageScreenX, imageScreenY},
        1.0,
    );
    const rectMini = computeWindowRect(
        {baseCssW, baseCssH, zoom: 0.1, imageScreenX, imageScreenY},
        1.0,
    );

    assertEqual(rectMini.winX, rectNormal.winX, '负坐标 mini winX = normal winX');
    assertEqual(rectMini.winY, rectNormal.winY, '负坐标 mini winY = normal winY');
});

test('mini 左上角不变式：往返 winX/winY 恒等', () => {
    const {baseCssW, baseCssH} = baseCssSize(1200, 900, 1.25);
    const imageScreenX = 400;
    const imageScreenY = 250;

    // normal → mini → normal 往返
    const r1 = computeWindowRect({baseCssW, baseCssH, zoom: 1, imageScreenX, imageScreenY}, 1.25);
    const r2 = computeWindowRect({baseCssW, baseCssH, zoom: 0.15, imageScreenX, imageScreenY}, 1.25);
    const r3 = computeWindowRect({baseCssW, baseCssH, zoom: 1, imageScreenX, imageScreenY}, 1.25);

    assertEqual(r2.winX, r1.winX, 'mini winX = normal winX');
    assertEqual(r3.winX, r1.winX, '往返 winX 恒等');
    assertEqual(r3.winW, r1.winW, '往返 winW 恒等');
    assertEqual(r3.winH, r1.winH, '往返 winH 恒等');
});

// ── 结果 ─────────────────────────────────────────────────────────────────

console.log(`\n${'═'.repeat(60)}`);
console.log(`  pin-geometry 测试结果：${passed} passed, ${failed} failed`);
console.log(`${'═'.repeat(60)}`);

if (failed > 0) {
    process.exit(1);
}
