//! Pin mini 落位可见性钳制结构契约（0.25.26）。
//!
//! 长图 pin 左上角可被 JS 拖动推到屏外（0.24.10 设计行为，否则长图下方
//! 内容不可达），双击 mini 若仍按左上角锚定，缩小后的小窗会整体落在屏外
//! ——表现为"双击后 pin 消失"。本文件用结构断言锁定修复的关键行为：
//! - enterMiniMode 落位前异步查询工作区并钳制 mini 矩形锚点
//! - 钳制发生时同步改写 imageScreenX/Y（防 onMoved→reconcile 安全网回拉）
//! - await 后校验动画代际（竞态防护铁则 spec-frontend §5.1）
//! - 钳制后跳过动画直接落位（屏外旧锚点的 CSS 动画不可见）
//!
//! 运行：node frontend/js/pin/pin-mini-clamp.test.mjs

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

const enterBody = bodyOf('async function enterMiniMode');
const clampBody = bodyOf('async function clampMiniAnchor');

describe('Pin mini 落位可见性钳制（0.25.26）', () => {
    test('enterMiniMode 落位前先经 clampMiniAnchor 钳制锚点', () => {
        assert.match(enterBody, /const relocated = await clampMiniAnchor\(miniZoom, gen\)/);
    });

    test('钳制作用于 mini 窗口矩形并同步改写锚点状态', () => {
        assert.match(clampBody, /clampWinOriginToWorkArea\(rect, wa\)/);
        assert.match(clampBody, /imageScreenX = clamped\.winX \+ rect\.padPhys/);
        assert.match(clampBody, /imageScreenY = clamped\.winY \+ rect\.padPhys/);
    });

    test('工作区查询失败降级为原左上角锚定（不阻断 mini）', () => {
        assert.match(clampBody, /catch \(e\)/);
        assert.match(clampBody, /if \(!wa \|\| gen !== miniAnimGen\) return false/);
    });

    test('await 后校验动画代际，旧会话不得落位（竞态防护）', () => {
        assert.match(enterBody, /if \(gen !== miniAnimGen\) return/);
    });

    test('锚点被挪进可见区时跳过动画直接落位', () => {
        assert.match(enterBody, /if \(relocated \|\| window\.matchMedia/);
        assert.match(enterBody, /settleMini\(\)/);
    });

    test('钳制函数来自 pin-geometry 纯函数模块（显式 import）', () => {
        assert.match(html, /import \{[\s\S]*?clampWinOriginToWorkArea,[\s\S]*?\} from "\.\/js\/pin\/pin-geometry\.js";/);
    });
});
