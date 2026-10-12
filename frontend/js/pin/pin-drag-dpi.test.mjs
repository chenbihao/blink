//! Pin 长图 JS 拖动跨 DPI 换算结构契约（0.25.26）。
//!
//! WebView2 的 MouseEvent.screenX/Y 是 CSS 像素（main-window/contextmenu.js
//! 同一实测结论），0.24.10 的长图 JS 拖动当物理像素累加，高 DPI 屏拖动偏
//! 1/dpr、跨 DPI 屏跳变。本文件锁定修复关键行为：
//! - pointermove 的 jsDrag 分支经 applyJsDragMove 按当前屏 DPR 换算
//! - DPR 取 window.devicePixelRatio（拖动期实时渲染尺度），异常回退状态值
//! - 拖动会话状态携带 lastScreen/dpr 供跨屏翻转按段落账
//! - onScaleChanged 的 reconcile 在拖动期间跳过（中途回读回设与拖动流打架）
//!
//! 运行：node frontend/js/pin/pin-drag-dpi.test.mjs

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

const moveBody = bodyOf("document.addEventListener('pointermove'");
const scaleBody = bodyOf('tauriWin.onScaleChanged');

describe('Pin 长图 JS 拖动 DPI 换算（0.25.26）', () => {
    test('pointermove 的 jsDrag 分支经 applyJsDragMove 换算', () => {
        assert.match(moveBody, /applyJsDragMove\(jsDrag, e\.screenX, e\.screenY, currentDragDpr\(\)\)/);
    });

    test('DPR 基准取实时渲染尺度，异常回退状态值', () => {
        const helper = bodyOf('function currentDragDpr');
        assert.match(helper, /window\.devicePixelRatio \|\| monitorDpr \|\| 1/);
    });

    test('拖动会话状态携带 lastScreen/dpr 供跨屏翻转落账', () => {
        assert.match(moveBody, /lastScreenX: e\.screenX/);
        assert.match(moveBody, /dpr: currentDragDpr\(\)/);
    });

    test('旧实现的无换算公式不得回归', () => {
        assert.doesNotMatch(moveBody, /imageScreenX = jsDrag\.startImageX \+ \(e\.screenX - jsDrag\.startScreenX\)/);
    });

    test('onScaleChanged 的 reconcile 在拖动期间跳过', () => {
        assert.match(scaleBody, /if \(jsDrag \|\| nativeDragActive\) return;/);
    });

    test('换算函数来自 pin-geometry 纯函数模块（显式 import）', () => {
        assert.match(html, /import \{[\s\S]*?applyJsDragMove,[\s\S]*?\} from "\.\/js\/pin\/pin-geometry\.js";/);
    });
});
