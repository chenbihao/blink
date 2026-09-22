//! 0.23.19 回归锁：pin/剪贴板/历史图片编辑器内一次 ESC 整体退出。
//!
//! 背景：isScrollCaptureActive 曾把 `!!ss._imagePan` 直接算作"长截图活跃"，
//! 而 _imagePan 在所有 canvas 编辑器（含 pin/剪贴板/历史来源）都会被设置。
//! 结果编辑器内首按 ESC 被"退出长截图"分支吃掉，第二次才真正取消——
//! 该缺陷连续两轮逃过修复。
//!
//! 0.23.19-fix：决策逻辑抽为纯函数（ss-editor-policy.js），此处做**行为测试**；
//! 监听注册/接线层（window 捕获、doCancel 接线、tooltip 内部状态机、Alt 守卫）
//! node:test 无 DOM 起不了真实事件，保留少量源码断言兜底。

import {describe, test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {IMAGE_SOURCE} from './image-editor-session.js';
import {
    cancelIsImageEditorSession,
    editorEscExempt,
    isLongScreenshotPan,
    isTextEntryTarget,
} from './ss-editor-policy.js';

const scrollSource = readFileSync(new URL('./scroll/index.js', import.meta.url), 'utf8');
const indexSource = readFileSync(new URL('./index.js', import.meta.url), 'utf8');
const tooltipSource = readFileSync(new URL('./ss-tooltip.js', import.meta.url), 'utf8');
const toolbarSource = readFileSync(new URL('./ss-toolbar.js', import.meta.url), 'utf8');
const outputSource = readFileSync(new URL('./ss-output.js', import.meta.url), 'utf8');

// ── 行为测试：决策纯函数（ss-editor-policy.js） ───────────────────────────

describe('isLongScreenshotPan（ESC 不被长截图分支吃掉）', () => {
    test('普通编辑器的 _imagePan 不算长截图活跃', () => {
        for (const source of [IMAGE_SOURCE.PIN, IMAGE_SOURCE.CLIPBOARD, IMAGE_SOURCE.HISTORY, IMAGE_SOURCE.SCREENSHOT, IMAGE_SOURCE.NONE]) {
            assert.equal(isLongScreenshotPan(true, source), false, `${source} 编辑器的 pan 不是长截图`);
        }
    });

    test('仅长截图来源的 pan 算活跃；无 pan 不算', () => {
        assert.equal(isLongScreenshotPan(true, IMAGE_SOURCE.LONG_SCREENSHOT), true);
        assert.equal(isLongScreenshotPan(false, IMAGE_SOURCE.LONG_SCREENSHOT), false);
        assert.equal(isLongScreenshotPan(null, IMAGE_SOURCE.LONG_SCREENSHOT), false);
    });
});

describe('editorEscExempt（编辑器整体退出的例外态）', () => {
    test('IME 组合 / 取色器活跃 / 输入控件目标 → 豁免（交还各自 ESC 语义）', () => {
        assert.equal(editorEscExempt({isComposing: true, target: null}), true);
        assert.equal(editorEscExempt({isComposing: false, target: {}}, {eyedropperActive: true}), true);
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'INPUT'}}), true);
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'TEXTAREA'}}), true);
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'DIV', isContentEditable: true}}), true);
    });

    test('普通目标（canvas/div/button/null）不豁免——ESC 应整体退出', () => {
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'CANVAS'}}), false);
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'DIV'}}), false);
        assert.equal(editorEscExempt({isComposing: false, target: {tagName: 'BUTTON'}}), false);
        assert.equal(editorEscExempt({isComposing: false, target: null}), false);
    });
});

describe('isTextEntryTarget（Alt 快捷键让位范围）', () => {
    test('文本类输入让位：text/无 type/textarea/contentEditable', () => {
        assert.equal(isTextEntryTarget({tagName: 'INPUT', getAttribute: () => 'text'}), true);
        // 无 type 属性按 text 处理（Windows Alt 码输入场景）
        assert.equal(isTextEntryTarget({tagName: 'INPUT', getAttribute: () => null}), true);
        assert.equal(isTextEntryTarget({tagName: 'TEXTAREA'}), true);
        assert.equal(isTextEntryTarget({tagName: 'DIV', isContentEditable: true}), true);
    });

    test('非文本控件不让位：range/checkbox 等拖完后焦点滞留也应响应 Alt', () => {
        for (const type of ['range', 'checkbox', 'radio', 'button', 'submit', 'reset', 'file', 'image', 'color']) {
            assert.equal(
                isTextEntryTarget({tagName: 'INPUT', getAttribute: () => type}),
                false,
                `input[type=${type}] 应正常响应 Alt 快捷键`,
            );
        }
        assert.equal(isTextEntryTarget({tagName: 'BUTTON'}), false);
        assert.equal(isTextEntryTarget(null), false);
    });
});

describe('cancelIsImageEditorSession（取消路由）', () => {
    test('编辑器来源直接走编辑器取消分支', () => {
        for (const source of [IMAGE_SOURCE.CLIPBOARD, IMAGE_SOURCE.HISTORY, IMAGE_SOURCE.PIN]) {
            assert.equal(cancelIsImageEditorSession(source, false), true, `${source} 必须走 imageEditorCancel`);
        }
    });

    test('source 被翻写成 screenshot 后由 body 标记兜底（restore_demoted_pin 不被绕过）', () => {
        assert.equal(cancelIsImageEditorSession(IMAGE_SOURCE.SCREENSHOT, true), true);
        assert.equal(cancelIsImageEditorSession(IMAGE_SOURCE.SCREENSHOT, false), false);
        assert.equal(cancelIsImageEditorSession(IMAGE_SOURCE.NONE, false), false);
    });
});

// ── 装配层源码断言（node:test 无 DOM，锁注册与接线） ────────────────────────

describe('装配层接线（源码断言兜底）', () => {
    test('isScrollCaptureActive 委托 isLongScreenshotPan 判定', () => {
        const start = scrollSource.indexOf('export function isScrollCaptureActive()');
        assert.ok(start > 0, 'isScrollCaptureActive 必须存在');
        const body = scrollSource.slice(start, start + 700);
        assert.ok(
            body.includes('isLongScreenshotPan('),
            `isScrollCaptureActive 必须委托纯函数判定（_imagePan 带 source 门槛）：\n${body}`,
        );
    });

    test('编辑器 ESC 有窗口捕获层兜底（capture: true）且经例外判定', () => {
        const handlerStart = indexSource.indexOf("window.addEventListener('keydown', (e) => {", indexSource.indexOf('imageEditorEscExempt'));
        assert.ok(handlerStart > 0, '编辑器 ESC 的 window 捕获监听必须存在');
        const tail = indexSource.slice(handlerStart, handlerStart + 1200);
        assert.ok(tail.includes('}, true);'), '监听必须以 capture: true 注册');
        assert.ok(tail.includes('imageEditorEscExempt(e)'), '必须经过取色/输入豁免判定');
        assert.ok(tail.includes('doCancel()'), '命中后必须整体取消（doCancel）');
    });

    test('doCancel 路由委托 cancelIsImageEditorSession，且整体取消时收起 tooltip', () => {
        const start = outputSource.indexOf('export function doCancel()');
        assert.ok(start > 0, 'doCancel 必须存在');
        const body = outputSource.slice(start, start + 2200);
        assert.ok(
            body.includes('cancelIsImageEditorSession('),
            'doCancel 必须经纯函数路由到 imageEditorCancel',
        );
        assert.ok(
            body.includes('hideSsTooltip()'),
            '整体取消路径必须收起 tooltip——ESC 整体退出的 stopPropagation 会跳过 tooltip 自己的 ESC 监听',
        );
    });

    test('tooltip 首次迁移后仍可再触发：anchorOf 同时匹配 [title] 与 [data-tip]', () => {
        assert.match(
            tooltipSource,
            /closest\('\[title\], \[data-tip\]'\)/,
            'title 首次显示被迁移为 data-tip 后，选择器必须继续匹配该元素',
        );
    });

    test('tooltip 无锚点 pointerout 取消待显示计时（防幽灵气泡）', () => {
        const handlerStart = tooltipSource.indexOf("document.addEventListener('pointerout'");
        const handler = tooltipSource.slice(handlerStart, tooltipSource.indexOf('});', handlerStart));
        assert.ok(
            handler.includes('clearTimeout(showTimer)'),
            'pointerout 的无锚点分支必须取消 showTimer，防止指针离开后旧位置弹残留气泡',
        );
    });

    test('hideSsTooltip 同时清挂起的 hideTimer（防相邻锚点切换吞掉显示）', () => {
        const fnStart = tooltipSource.indexOf('function hideSsTooltip()');
        assert.ok(fnStart > 0, 'hideSsTooltip 必须存在且导出（doCancel 收起用）');
        const body = tooltipSource.slice(fnStart, tooltipSource.indexOf('}', tooltipSource.indexOf('{', fnStart)));
        assert.ok(
            body.includes('clearTimeout(hideTimer)'),
            'hideSsTooltip 必须清 hideTimer：A→B 相邻切换时旧 hideTimer 若存活，会在 B 的显示计时途中触发并连带清掉 showTimer',
        );
    });

    test('原生抑制：初始化即全量迁移 + MutationObserver 常驻（R3 竞态修复）', () => {
        assert.match(
            tooltipSource,
            /migrateTitlesIn\(document\.body\)/,
            'initSsTooltip 必须在初始化时全量迁移存量 [title]，而非等到首次显示',
        );
        assert.match(
            tooltipSource,
            /attributeFilter:\s*\['title'\]/,
            '必须用 MutationObserver 监听动态设置的 title 并即时迁移，否则动态设置 title 的按钮会弹原生提示',
        );
    });

    test('canvas 文字输入框激活时 Alt 组合 = blur 提交后继续执行快捷键', () => {
        const start = indexSource.indexOf('if (e.altKey && !e.ctrlKey && !e.metaKey && ss.isAnnotating)');
        assert.ok(start > 0, 'Alt 快捷键块必须存在');
        const body = indexSource.slice(start, start + 2000);
        assert.ok(
            body.includes("contains('text-annot-input')") && body.includes('tgt.blur()'),
            'text-annot-input 激活时 Alt 组合必须 blur（同步提交文本、移除输入框）后继续处理，不得直接 return',
        );
        assert.ok(
            body.includes('isComposing'),
            'Alt 快捷键块必须先排除 IME 组合中的按键',
        );
        assert.ok(
            body.includes('isTextEntryTarget('),
            '让位判定必须委托 isTextEntryTarget（行为测试在上方）',
        );
    });

    test('文字输入框 ESC = 落袋退出（有文本提交，空输入取消）', () => {
        const start = toolbarSource.indexOf("input.addEventListener('keydown', (e) => {");
        const body = toolbarSource.slice(start, start + 700);
        assert.ok(
            body.includes("e.key === 'Escape'") && body.includes('commit(getText())'),
            'ESC 分支必须走 commit(getText())——有文本落袋（Figma 语义），空输入取消；不得直接 cancelText 丢弃',
        );
    });
});
