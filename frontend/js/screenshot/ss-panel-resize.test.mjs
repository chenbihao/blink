//! OCR 面板缩放几何纯函数测试（0.22.7 新增）。
//!
//! 覆盖：
//! 1. clampPanelSize：min/max 钳制
//! 2. computeResizedPanel：拖拽位移 → 新尺寸（含 uiScale 补偿）
//! 3. clampPanelToMonitor：显示器边界钳制（右下角不越界）

import {
    clampPanelSize,
    computeResizedPanel,
    clampPanelToMonitor,
    PANEL_MIN_W,
    PANEL_MIN_H,
    PANEL_MAX_W,
    PANEL_MAX_H,
} from './ss-panel-resize.js';

// ── 断言辅助 ──────────────────────────────────────────────────────────────

function assertEqual(actual, expected, msg) {
    const aStr = JSON.stringify(actual);
    const eStr = JSON.stringify(expected);
    if (aStr !== eStr) {
        throw new Error(`${msg}: expected ${eStr}, got ${aStr}`);
    }
    console.log(`✓ ${msg}`);
}

function assertApprox(actual, expected, msg, tolerance = 0.01) {
    if (Math.abs(actual - expected) > tolerance) {
        throw new Error(`${msg}: expected ${expected}, got ${actual}`);
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

// ── clampPanelSize ──────────────────────────────────────────────────────────

test('clampPanelSize: 正常值不钳制', () => {
    const r = clampPanelSize(400, 300);
    assertEqual(r, {w: 400, h: 300}, '正常尺寸应原样返回');
});

test('clampPanelSize: 过小值钳制到最小', () => {
    const r = clampPanelSize(100, 50);
    assertEqual(r, {w: PANEL_MIN_W, h: PANEL_MIN_H}, '过小值应钳制到最小');
});

test('clampPanelSize: 过大值钳制到最大', () => {
    const r = clampPanelSize(9999, 9999);
    assertEqual(r, {w: PANEL_MAX_W, h: PANEL_MAX_H}, '过大值应钳制到最大');
});

test('clampPanelSize: 宽度过小但高度正常', () => {
    const r = clampPanelSize(100, 300);
    assertEqual(r, {w: PANEL_MIN_W, h: 300}, '只钳制宽度');
});

test('clampPanelSize: 等于最小值时不钳制', () => {
    const r = clampPanelSize(PANEL_MIN_W, PANEL_MIN_H);
    assertEqual(r, {w: PANEL_MIN_W, h: PANEL_MIN_H}, '等于最小值应原样返回');
});

test('clampPanelSize: 等于最大值时不钳制', () => {
    const r = clampPanelSize(PANEL_MAX_W, PANEL_MAX_H);
    assertEqual(r, {w: PANEL_MAX_W, h: PANEL_MAX_H}, '等于最大值应原样返回');
});

// ── computeResizedPanel ─────────────────────────────────────────────────────

test('computeResizedPanel: uiScale=1 时位移直接映射', () => {
    const r = computeResizedPanel(400, 300, 50, 30, 1);
    assertEqual(r, {w: 450, h: 330}, 'uiScale=1 时 delta 直接加');
});

test('computeResizedPanel: uiScale=2 时位移除以 2', () => {
    // 视觉位移 100px，uiScale=2 → 布局位移 50px
    const r = computeResizedPanel(400, 300, 100, 60, 2);
    assertEqual(r, {w: 450, h: 330}, 'uiScale=2 时 delta/2');
});

test('computeResizedPanel: 负位移（缩小）', () => {
    const r = computeResizedPanel(400, 300, -100, -50, 1);
    assertEqual(r, {w: 300, h: 250}, '负位移应缩小尺寸');
});

test('computeResizedPanel: 缩到低于最小值时钳制', () => {
    const r = computeResizedPanel(300, 250, -500, -500, 1);
    assertEqual(r, {w: PANEL_MIN_W, h: PANEL_MIN_H}, '过度缩小应钳制到最小');
});

test('computeResizedPanel: 放到超过最大值时钳制', () => {
    const r = computeResizedPanel(700, 600, 500, 500, 1);
    assertEqual(r, {w: PANEL_MAX_W, h: PANEL_MAX_H}, '过度放大应钳制到最大');
});

test('computeResizedPanel: uiScale=1.5 时的混合补偿', () => {
    // 视觉位移 45px → 布局位移 30px
    const r = computeResizedPanel(400, 300, 45, 30, 1.5);
    assertApprox(r.w, 430, 'uiScale=1.5 时 w=400+30=430');
    assertApprox(r.h, 320, 'uiScale=1.5 时 h=300+20=320');
});

// ── clampPanelToMonitor ─────────────────────────────────────────────────────

test('clampPanelToMonitor: 面板完全在屏内不收束', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    const r = clampPanelToMonitor(100, 100, 400, 300, 1, mon);
    assertEqual(r, {w: 400, h: 300}, '面板在屏内应原样返回');
});

test('clampPanelToMonitor: 右下角越界时收束尺寸', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    // left=1800, w=400 → 右边界=2200 > 1920-8=1912
    const r = clampPanelToMonitor(1800, 100, 400, 300, 1, mon);
    // maxVisW = 1920 - 8 - 1800 = 112
    // 但 PANEL_MIN_W * 1 = 280 > 112，所以 allowedVisW = 280
    // clampedVisW = min(400, 280) = 280
    assertEqual(r.w, PANEL_MIN_W, '宽度应收束到最小值（空间不够时）');
});

test('clampPanelToMonitor: uiScale=2 时视觉尺寸用于边界判断', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    // left=1000, w=400, uiScale=2 → visW=800 → 右边界=1800 < 1912 → 不越界
    const r = clampPanelToMonitor(1000, 100, 400, 300, 2, mon);
    assertEqual(r, {w: 400, h: 300}, 'uiScale=2 且不越界时应原样返回');
});

test('clampPanelToMonitor: uiScale=2 越界时收束', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    // left=1500, w=400, uiScale=2 → visW=800 → 右边界=2300 > 1912 → 越界
    const r = clampPanelToMonitor(1500, 100, 400, 300, 2, mon);
    // maxVisW = 1920 - 8 - 1500 = 412
    // PANEL_MIN_W * 2 = 560 > 412 → allowedVisW = 560
    // clampedVisW = min(800, 560) = 560 → newW = 560/2 = 280
    assertEqual(r.w, PANEL_MIN_W, 'uiScale=2 越界应收束到最小值');
});

test('clampPanelToMonitor: 自定义 margin 且面板小于最小值时仍钳制到最小', () => {
    const mon = {x: 0, y: 0, w: 1920, h: 1080};
    // left=1850, w=100, uiScale=1, margin=20 → maxVisW=1920-20-1850=50
    // 但 w=100 < PANEL_MIN_W=280，所以 clampPanelSize 会拉到 280
    const r = clampPanelToMonitor(1850, 100, 100, 200, 1, mon, 20);
    assertEqual(r, {w: PANEL_MIN_W, h: PANEL_MIN_H}, '面板小于最小值时仍钳制到最小');
});

// ── 汇总 ───────────────────────────────────────────────────────────────────

console.log(`\n${passed}/${total} tests passed`);
if (passed !== total) {
    process.exitCode = 1;
}
