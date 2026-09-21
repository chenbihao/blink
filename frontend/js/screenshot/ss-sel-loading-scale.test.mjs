//! sel-loading（选区中央"识别中/翻译中"指示器）DPI 视觉补偿测试。
//!
//! 背景：sel-loading 是截图 overlay 里唯一漏掉 uiScale 补偿的浮层（其余
//! toolbar / sizeHint / magnifier / errorHint / reading 菜单均已按
//! spec-frontend §5.6 做 uiScale = targetMonitorDpr / renderScale）。
//! 混合 DPI 下（如 overlay renderScale=1.5、选区落在 100% 副屏）指示器
//! 物理尺寸会偏大 1.5 倍。此测试钉死：
//! - transform 组合 translate(-50%,-50%) 与 scale(uiScale)，中心仍精确落在选区中心
//! - uiScale 来自选区中心所在屏（换屏后跟着变）

import {test} from 'node:test';
import assert from 'node:assert';

// ── 浏览器全局 mock（必须在 import 生产模块前就位）────────────────────────
globalThis.window = globalThis;
const fakeLabel = {textContent: ''};
const fakeEl = {
    style: {},
    hidden: true,
    querySelector: (sel) => (sel === '.sel-loading-text' ? fakeLabel : null),
};
globalThis.document = {
    getElementById: (id) => (id === 'sel-loading' ? fakeEl : null),
};

const {showSelLoading} = await import('./ss-ocr.js');
const {ss} = await import('./ss-state.js');
const {uiScaleAtCss} = await import('./ss-selection-geometry.js');

// 场景：overlay 窗口 renderScale=1.5（主屏 150%），两块屏：
// - 主屏 150%（dpi 144）物理 [0, 2880)
// - 副屏 100%（dpi 96）物理 [2880, 2880+1920)
// CSS → screen：css × 1.5；CSS x ∈ [1920, 3200) 落在副屏
const META = {
    vx: 0, vy: 0, w: 4800, h: 1080,
    renderScaleX: 1.5, renderScaleY: 1.5,
    physicalDisplays: [
        {x: 0, y: 0, w: 2880, h: 1080, dpi: 144, primary: true},
        {x: 2880, y: 0, w: 1920, h: 1080, dpi: 96, primary: false},
    ],
};

function setupSelection(cssX, cssY) {
    ss.selCss = {x: cssX, y: cssY, w: 200, h: 100};
    globalThis.__blinkScreenMeta = META;
}

test('sel-loading 在 100% 副屏：uiScale = 1/1.5，中心精确落在选区中心', () => {
    setupSelection(2200, 200); // 选区中心 CSS (2300, 250) → screen (3450, 375) 落副屏
    showSelLoading('翻译中…');

    const cx = ss.selCss.x + ss.selCss.w / 2;
    const cy = ss.selCss.y + ss.selCss.h / 2;
    assert.equal(fakeEl.style.left, cx + 'px', 'left = 选区中心 X');
    assert.equal(fakeEl.style.top, cy + 'px', 'top = 选区中心 Y');

    const expectedScale = uiScaleAtCss(cx, cy, META);
    assert.ok(Math.abs(expectedScale - 1 / 1.5) < 0.001, `uiScale 应为 1/1.5，实际 ${expectedScale}`);
    assert.equal(
        fakeEl.style.transform,
        `translate(-50%, -50%) scale(${expectedScale})`,
        'transform 组合居中平移与视觉补偿缩放',
    );
    assert.equal(fakeEl.hidden, false, '指示器显示');
    assert.equal(fakeLabel.textContent, '翻译中…', '文案透传');
});

test('sel-loading 回到 150% 主屏：uiScale = 1（无补偿）', () => {
    setupSelection(100, 100); // 选区中心 CSS (200, 150) → screen (300, 225) 落主屏
    showSelLoading('识别中…');

    const cx = ss.selCss.x + ss.selCss.w / 2;
    const cy = ss.selCss.y + ss.selCss.h / 2;
    const expectedScale = uiScaleAtCss(cx, cy, META);
    assert.ok(Math.abs(expectedScale - 1) < 0.001, `uiScale 应为 1，实际 ${expectedScale}`);
    assert.equal(fakeEl.style.transform, `translate(-50%, -50%) scale(${expectedScale})`);
    assert.equal(fakeLabel.textContent, '识别中…');
});

test('meta 缺失时安全降级（uiScale 走 fallback 链，不抛错）', () => {
    ss.selCss = {x: 10, y: 10, w: 50, h: 50};
    delete globalThis.__blinkScreenMeta;
    fakeEl.style.transform = '';
    showSelLoading('翻译中…');
    assert.equal(fakeEl.hidden, false, '无 meta 也正常显示');
    assert.match(fakeEl.style.transform, /translate\(-50%, -50%\) scale\(/);
    globalThis.__blinkScreenMeta = META;
});
