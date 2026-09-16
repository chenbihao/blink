//! OCR 面板落位纯函数测试（0.23.12 新增）。
//!
//! 覆盖 placeOcrPanel 的选区遮挡规避：
//! 1. 常规落位（工具栏在选区下方）→ 锚点下方
//! 2. 选区贴底（工具栏翻到选区上方）→ 翻到锚点上方，不压选区
//! 3. 锚点贴近屏顶无法上翻 → 落到选区下方
//! 4. 全屏选区全部候选被压 → 退化为候选 1 的钳制位置
//! 5. selRect=null → 不做遮挡规避
//! 6. 显示器边界钳制（含负坐标副屏、面板超出屏幕）

import {placeOcrPanel} from './ss-panel-resize.js';

function assertEqual(actual, expected, msg) {
    const aStr = JSON.stringify(actual);
    const eStr = JSON.stringify(expected);
    if (aStr !== eStr) {
        throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
    }
    console.log(`✓ ${msg}`);
}

let passed = 0;
let total = 0;

function test(name, fn) {
    total++;
    try {
        fn();
        passed++;
    } catch (e) {
        console.error(`✗ ${name}: ${e.message}`);
        process.exitCode = 1;
    }
}

// ── 常规落位 ────────────────────────────────────────────────────────────────

test('placeOcrPanel: 工具栏在选区下方且下方放得下 → 锚点下方', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const sel = {x: 500, y: 200, w: 400, h: 300};
    // 工具栏贴选区下方（bottom=552）
    const anchor = {left: 800, top: 504, right: 900, bottom: 552};
    const p = placeOcrPanel(anchor, 360, 480, mon, sel);
    assertEqual(p, {left: 800, top: 556}, '常规情况应落在锚点下方 + gap');
});

test('placeOcrPanel: 选区贴底、工具栏已翻到选区上方 → 面板翻到锚点上方不压选区', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const sel = {x: 500, y: 700, w: 400, h: 300};
    // 工具栏在选区上方（positionToolbar 的 above 分支）
    const anchor = {left: 800, top: 640, right: 900, bottom: 688};
    const p = placeOcrPanel(anchor, 360, 480, mon, sel);
    assertEqual(p, {left: 800, top: 156}, '应翻到锚点上方（640-480-4=156）');
    // 落位不得与选区相交
    const noOverlap = p.top + 480 <= sel.y || p.top >= sel.y + sel.h
        || p.left + 360 <= sel.x || p.left >= sel.x + sel.w;
    assertEqual(noOverlap, true, '落位面板不应与选区相交');
});

test('placeOcrPanel: 锚点贴近屏顶无法上翻 → 落到选区下方', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const sel = {x: 500, y: 150, w: 400, h: 300};
    // 工具栏翻到选区上方且贴近屏顶
    const anchor = {left: 800, top: 60, right: 900, bottom: 108};
    const p = placeOcrPanel(anchor, 360, 480, mon, sel);
    assertEqual(p, {left: 500, top: 454}, '应落到选区下方（450+4=454）');
});

// ── 退化与兜底 ──────────────────────────────────────────────────────────────

test('placeOcrPanel: 全屏选区全部候选被压 → 退化为锚点下方钳制位置', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const sel = {x: 0, y: 0, w: 1920, h: 1080};
    // 工具栏浮入选区内部、贴近底部
    const anchor = {left: 800, top: 1000, right: 900, bottom: 1048};
    const p = placeOcrPanel(anchor, 360, 480, mon, sel);
    // top 钳制到 1080-8-480=592
    assertEqual(p, {left: 800, top: 592}, '退化时应钳制回屏内');
});

test('placeOcrPanel: selRect=null 时不做遮挡规避', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const anchor = {left: 1900, top: 500, right: 2000, bottom: 548};
    const p = placeOcrPanel(anchor, 360, 480, mon, null);
    // left 钳制到 1920-8-360=1552
    assertEqual(p, {left: 1552, top: 552}, '无选区时直接取锚点下方钳制位');
});

// ── 边界钳制 ────────────────────────────────────────────────────────────────

test('placeOcrPanel: 负坐标副屏内正常钳制', () => {
    const mon = {x: -1920, y: 0, w: 1920, h: 1080};
    const anchor = {left: -100, top: 452, right: 0, bottom: 500};
    const p = placeOcrPanel(anchor, 360, 480, mon, null);
    // maxLeft = -1920+1920-8-360 = -368
    assertEqual(p, {left: -368, top: 504}, '副屏负坐标应正确钳制');
});

test('placeOcrPanel: 面板高度超过屏幕时钳制到顶部 margin', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 400};
    const anchor = {left: 100, top: 100, right: 200, bottom: 148};
    const p = placeOcrPanel(anchor, 360, 480, mon, null);
    // maxTop = max(8, 400-8-480) = 8
    assertEqual(p, {left: 100, top: 8}, '超高面板应钳制到屏顶 margin');
});

test('placeOcrPanel: 右下角无避让空间时钳制后仍重叠选区 → 退化候选1钳制位', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    // 选区占据右上方区域；锚点在选区下方但面板高度放不下剩余空间
    const sel = {x: 1200, y: 0, w: 720, h: 600};
    const anchor = {left: 1500, top: 604, right: 1600, bottom: 652};
    const p = placeOcrPanel(anchor, 360, 480, mon, sel);
    // 候选1 (1500,656) top 钳制到 592 后与选区 y∈[0,600) 重叠 8px，候选 2/3/4 同样被压；
    // 退化为候选 1 的钳制位置（left=1500 ≤ maxLeft=1552，top=592=maxTop）
    assertEqual(p, {left: 1500, top: 592}, '全部候选被压时退化为候选1钳制位');
});

// ── 汇总 ───────────────────────────────────────────────────────────────────

console.log(`\n${passed}/${total} tests passed`);
if (passed !== total) {
    process.exitCode = 1;
}
