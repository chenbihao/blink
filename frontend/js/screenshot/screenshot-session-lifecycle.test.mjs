//! 截图窗口复用生命周期回归：紧急 ESC 只能在正常模块未就绪时生效。

import {describe, test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';

const htmlUrl = new URL('../../chord-screenshot.html', import.meta.url);
const html = readFileSync(htmlUrl, 'utf8');
const indexSource = readFileSync(new URL('./index.js', import.meta.url), 'utf8');
const outputSource = readFileSync(new URL('./ss-output.js', import.meta.url), 'utf8');
const inlineScripts = [...html.matchAll(/^\s*<script>\s*\r?\n([\s\S]*?)^\s*<\/script>\s*$/gm)]
    .map((match) => match[1]);
const emergencyScript = inlineScripts.find((source) => source.includes('__blinkDisableEmergencyScreenshotEscape'));

function createHarness() {
    const listeners = new Map();
    const classes = new Set();
    const invokes = [];
    const document = {
        documentElement: {
            classList: {
                add: (name) => classes.add(name),
                remove: (name) => classes.delete(name),
                contains: (name) => classes.has(name),
            },
        },
        addEventListener(type, handler) {
            if (!listeners.has(type)) listeners.set(type, new Set());
            listeners.get(type).add(handler);
        },
        removeEventListener(type, handler) {
            listeners.get(type)?.delete(handler);
        },
    };
    const window = {
        location: {search: '?preheat=1'},
        __TAURI_INTERNALS__: {
            invoke(command) {
                invokes.push(command);
                return Promise.resolve();
            },
        },
    };
    vm.runInNewContext(emergencyScript, {
        document,
        window,
        console,
        setTimeout: () => 1,
    });
    return {
        classes,
        invokes,
        window,
        dispatchKey(key) {
            let prevented = false;
            const event = {
                key,
                preventDefault() {
                    prevented = true;
                },
            };
            for (const handler of listeners.get('keydown') || []) handler(event);
            return {prevented};
        },
    };
}

describe('截图 session 紧急 ESC', () => {
    test('HTML 初始就处于不可见会话态', () => {
        assert.match(html, /<html\s+class="screenshot-session-inactive"/);
    });

    test('模块未就绪时先标记不可见，再调后端 hide', () => {
        const harness = createHarness();
        const result = harness.dispatchKey('Escape');
        assert.equal(result.prevented, true);
        assert.equal(harness.classes.has('screenshot-session-inactive'), true);
        assert.deepEqual(harness.invokes, ['hide_screenshot_overlay']);
    });

    test('正常模块就绪后完全移除紧急 handler', () => {
        const harness = createHarness();
        harness.window.__blinkDisableEmergencyScreenshotEscape();
        harness.classes.delete('screenshot-session-inactive');

        const result = harness.dispatchKey('Escape');
        assert.equal(result.prevented, false);
        assert.equal(harness.classes.has('screenshot-session-inactive'), false);
        assert.deepEqual(harness.invokes, []);
        assert.equal(harness.window.__blinkDisableEmergencyScreenshotEscape, null);
    });

    test('非 ESC 不触发容灾退出', () => {
        const harness = createHarness();
        const result = harness.dispatchKey('Enter');
        assert.equal(result.prevented, false);
        assert.deepEqual(harness.invokes, []);
    });
});

describe('截图 session 可见性与代际契约', () => {
    test('新会话必须先同步清理，再恢复可见', () => {
        const start = indexSource.indexOf('window.__blinkStartScreenshotSession = function');
        const end = indexSource.indexOf('window.__blinkReloadScreenshot = function', start);
        const body = indexSource.slice(start, end);
        const resetCall = body.indexOf('window.__blinkClearScreenshotVisual();');
        const showCall = body.indexOf("classList.remove('screenshot-session-inactive')");
        assert.ok(resetCall >= 0 && showCall > resetCall);
    });

    test('正常模块只在交互绑定完成后禁用紧急 ESC', () => {
        const bind = indexSource.lastIndexOf('bindToolbar();');
        const disable = indexSource.lastIndexOf('__blinkDisableEmergencyScreenshotEscape?.();');
        assert.ok(bind >= 0 && disable > bind);
    });

    test('退出先使 OCR/翻译代际失效，再取消在途 OCR', () => {
        const start = outputSource.indexOf('export function doCancel()');
        const body = outputSource.slice(start);
        const selectionInvalidation = body.indexOf('ss.selectionRevision++;');
        const translationInvalidation = body.indexOf('ss.translationRevision++;');
        const cancel = body.indexOf('cancelActiveOcr();');
        assert.ok(selectionInvalidation >= 0 && translationInvalidation > selectionInvalidation);
        assert.ok(cancel > translationInvalidation);
    });
});
