//! Pin 右键菜单交互链测试（源码断言，与 pin-session-lifecycle.test.mjs 同模式）。
//!
//! 背景：多屏实机上"右键展开菜单 / 左键取消菜单"偶发要按两次。定位到
//! 四个吃第一次点击的机制，此文件钉死它们的修复形态不被回归：
//! 1. suppressClick 永久布尔标志 → 限时窗口（suppressClickUntil）
//! 2. contextmenu 拖动后 200ms 静默窗口 → 80ms
//! 3. 右键落在已打开菜单上静默忽略 → 重定位
//! 4. 菜单 dismiss 等 click（松开才关，可能被吞）→ pointerdown 按下即关
//! 5. 小 pin 菜单显示不全 → 菜单打开期间临时扩窗（expandRectForMenu）

import {describe, test} from 'node:test';
import assert from 'node:assert';
import {readFileSync} from 'node:fs';

const html = readFileSync(new URL('../../pin.html', import.meta.url), 'utf8');

function bodyOf(name) {
    const start = html.indexOf(name);
    assert.ok(start >= 0, `找不到 ${name}`);
    const nextSection = html.indexOf('\n    // ──', start + name.length);
    return html.slice(start, nextSection >= 0 ? nextSection : html.length);
}

describe('Pin 右键菜单交互链', () => {
    test('suppressClick 是限时窗口而非永久布尔，过期不吞真实点击', () => {
        assert.doesNotMatch(html, /let suppressClick = false/, '不得存在永久 suppressClick 布尔标志');
        const clickHandler = bodyOf("document.addEventListener('click', (e) => {");
        assert.match(clickHandler, /Date\.now\(\) >= suppressClickUntil/, 'click 处理器按时间判断过期');
        assert.match(clickHandler, /suppressClickUntil = 0/, '吞掉一次后立即清零');
    });

    test('残余 click 吞除窗口在拖动结束两处设置（pointerup + watchdog）', () => {
        assert.equal(
            (html.match(/suppressClickUntil = Date\.now\(\) \+ 250/g) || []).length,
            2,
            'pointerup 与 dragEndWatchdog 各设置一次',
        );
    });

    test('contextmenu 拖动后静默窗口收窄到 80ms，右键落菜单上重定位', () => {
        const ctx = bodyOf("document.addEventListener('contextmenu', (e) => {");
        assert.match(ctx, /dragEndTime < 80/, '守卫收窄到 80ms');
        assert.doesNotMatch(ctx, /e\.target\.closest\('#ctx-menu'\)\) return/, '右键落菜单不得静默忽略');
        assert.match(ctx, /showCtxMenu\(e\.clientX, e\.clientY\)/, '右键统一走 showCtxMenu 重定位');
    });

    test('菜单 dismiss 走 pointerdown 按下即关，不依赖 click', () => {
        const pd = bodyOf("document.addEventListener('pointerdown', (e) => {");
        assert.match(pd, /isCtxOpen && !e\.target\.closest\('#ctx-menu'\)/, '菜单外左键按下即关');
        // 旧的 click-dismiss 监听必须移除（双通道会互相打架）
        assert.doesNotMatch(
            html,
            /document\.addEventListener\('click', \(e\) => \{\s*\n\s*if \(isCtxOpen && !e\.target\.closest\('#ctx-menu'\)\) \{\s*\n\s*hideCtxMenu\(\);/,
            '不得保留旧的 click dismiss 监听',
        );
    });

    test('原生拖动启动时收起菜单', () => {
        const pm = bodyOf("document.addEventListener('pointermove', (e) => {");
        assert.match(pm, /if \(isCtxOpen\) hideCtxMenu\(\)/, 'startDragging 前关菜单');
    });

    test('小 pin 菜单临时扩窗：commitWindowRect 与 reconcile 都应用 expandRectForMenu', () => {
        assert.match(bodyOf('function commitWindowRect'), /expandRectForMenu\(/, '提交矩形先扩窗');
        const rec = bodyOf('async function reconcileFromBackend');
        assert.match(rec, /expandRectForMenu\(result, ctxExpand/, 'reconcile 比较用扩窗后的矩形');
        assert.match(rec, /result\.imageScreenX \+ shiftX/, '扩窗平移期间图片坐标反推要加回 shift');
    });

    test('扩窗激活时菜单关闭还原窗口，会话重置清空扩窗状态', () => {
        const hide = bodyOf('function hideCtxMenu');
        assert.match(hide, /if \(ctxExpand\)/, '只在扩窗激活时还原');
        assert.match(hide, /commitWindowRect\(\)/, '还原要提交常规矩形');
        assert.match(bodyOf('window.__blinkResetPin = function'), /resetCtxExpansion\(\)/, '重置入口清扩窗');
        assert.match(bodyOf('window.__blinkClearPin = function'), /resetCtxExpansion\(\)/, '回收入口清扩窗');
    });

    test('mini 动画与 DPI 切换前先收菜单', () => {
        assert.match(bodyOf('function enterMiniMode'), /if \(isCtxOpen\) hideCtxMenu\(\)/, 'enterMini 先关菜单');
        assert.match(bodyOf('function exitMiniMode'), /if \(isCtxOpen\) hideCtxMenu\(\)/, 'exitMini 先关菜单');
        assert.match(bodyOf('tauriWin.onScaleChanged'), /if \(isCtxOpen\) hideCtxMenu\(\)/, 'DPI 变化先还原扩窗');
    });

    test('扩窗期间图片视觉位置不动（pad + shift CSS 补偿）', () => {
        assert.match(bodyOf('function setImgAnchor'), /PIN_PAD_CSS \+ menuShiftCss\.x/, '图片锚点含平移补偿');
        assert.match(bodyOf('function pinTextLayerGeometry'), /PIN_PAD_CSS \+ menuShiftCss\.x/, '文字图层 pad 同步补偿');
        assert.match(bodyOf('function updatePinIndicatorPosition'), /PIN_PAD_CSS \+ menuShiftCss\.x/, '指示器中心同步补偿');
    });
});
