//! OCR 按钮呼吸动效（方案 A）测试 —— import 生产模块 ss-ocr-busy.js。
//!
//! 覆盖：
//! - 预热在途（ocrPrewarmActive）→ 延迟 200ms 后挂 is-busy
//! - ocrBusy → 同样点亮
//! - 短任务防闪烁：延迟窗口内解除 → timer 取消，class 永不出现
//! - 延迟到期前已取消 → 再校验兜底，不加 class
//! - 解除立即摘 class（识别结束动效即消失）
//! - 无 #btn-ocr（预热窗口）静默跳过
//! - 生命周期接线（源码断言，lifecycle 测试同风格）：
//!   cancelActiveOcr 清 ocrPrewarmActive 并同步动效；updateOutputButtonsDisabled 收口翻转

import {describe, test, beforeEach} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';

// ── DOM / timer mock（须在 import 生产代码前就位）──────────────────────────

const btnClasses = new Set();
const fakeBtn = {
    classList: {
        add: (name) => btnClasses.add(name),
        remove: (name) => btnClasses.delete(name),
        contains: (name) => btnClasses.has(name),
    },
};

let pendingTimer = null;   // {fn, delay} | null
let timerCleared = 0;

globalThis.document = {
    getElementById(id) {
        return id === 'btn-ocr' ? fakeBtn : null;
    },
};
globalThis.setTimeout = (fn, delay) => {
    pendingTimer = {fn, delay};
    return 1;
};
globalThis.clearTimeout = () => {
    timerCleared++;
    pendingTimer = null;
};

const {ss} = await import('./ss-state.js');
const {updateOcrButtonBusy} = await import('./ss-ocr-busy.js');

function resetBusyState() {
    ss.ocrBusy = false;
    ss.ocrPrewarmActive = false;
    ss.ocrPrewarm = null;
    btnClasses.delete('is-busy');
    pendingTimer = null;
    timerCleared = 0;
}

beforeEach(() => {
    resetBusyState();
});

describe('updateOcrButtonBusy —— 状态到 class 的映射', () => {
    test('预热在途：200ms 延迟后挂 is-busy', () => {
        ss.ocrPrewarmActive = true;
        updateOcrButtonBusy();
        assert.equal(btnClasses.has('is-busy'), false, '延迟窗口内不应立即挂 class');
        assert.equal(pendingTimer.delay, 200);
        pendingTimer.fn();
        assert.equal(btnClasses.has('is-busy'), true, '延迟到期后应挂 class');
    });

    test('ocrBusy：同样点亮', () => {
        ss.ocrBusy = true;
        updateOcrButtonBusy();
        pendingTimer.fn();
        assert.equal(btnClasses.has('is-busy'), true);
    });

    test('延迟窗口内解除：timer 取消，class 永不出现（短任务防闪烁）', () => {
        ss.ocrPrewarmActive = true;
        updateOcrButtonBusy();
        ss.ocrPrewarmActive = false;
        updateOcrButtonBusy();
        assert.equal(timerCleared, 1, '应取消待触发 timer');
        assert.equal(pendingTimer, null);
        pendingTimer?.fn?.();
        assert.equal(btnClasses.has('is-busy'), false, '已取消的 timer 不应挂 class');
    });

    test('延迟到期前已取消：回调内再校验兜底', () => {
        ss.ocrPrewarmActive = true;
        updateOcrButtonBusy();
        const fn = pendingTimer.fn;
        ss.ocrPrewarmActive = false;
        fn();
        assert.equal(btnClasses.has('is-busy'), false, '到期但忙碌态已解除，不应挂 class');
    });

    test('识别结束：立即摘 class', () => {
        ss.ocrBusy = true;
        updateOcrButtonBusy();
        pendingTimer.fn();
        assert.equal(btnClasses.has('is-busy'), true);
        ss.ocrBusy = false;
        updateOcrButtonBusy();
        assert.equal(btnClasses.has('is-busy'), false, '解除应立即摘 class，不等延迟');
    });

    test('无 #btn-ocr：静默跳过不抛错', () => {
        globalThis.document.getElementById = () => null;
        try {
            ss.ocrPrewarmActive = true;
            assert.doesNotThrow(() => updateOcrButtonBusy());
        } finally {
            globalThis.document.getElementById = (id) => id === 'btn-ocr' ? fakeBtn : null;
        }
    });
});

// ── 生命周期接线（源码断言，与 screenshot-session-lifecycle.test.mjs 同风格）──

const ocrSource = readFileSync(new URL('./ss-ocr.js', import.meta.url), 'utf8');
const indexSource = readFileSync(new URL('./index.js', import.meta.url), 'utf8');

describe('OCR 忙碌态生命周期接线', () => {
    test('cancelActiveOcr：清 ocrPrewarmActive 并同步熄灭动效', () => {
        const start = ocrSource.indexOf('export function cancelActiveOcr()');
        const end = ocrSource.indexOf('export function', start + 10);
        const body = ocrSource.slice(start, end);
        const clear = body.indexOf('ss.ocrPrewarmActive = false;');
        const sync = body.indexOf('updateOcrButtonBusy();');
        assert.ok(clear >= 0, 'cancelActiveOcr 应清除预热在途标志');
        assert.ok(sync > clear, '清除后应同步动效状态');
    });

    test('updateOutputButtonsDisabled：所有 ocrBusy 翻转收口处同步动效', () => {
        const start = ocrSource.indexOf('export function updateOutputButtonsDisabled()');
        const end = ocrSource.indexOf('export function', start + 10);
        const body = ocrSource.slice(start, end);
        assert.ok(body.includes('updateOcrButtonBusy();'), '集中收口应调用 updateOcrButtonBusy');
    });

    test('triggerOcrPrewarm：busy 点亮/熄灭受 handle 生命周期守卫', () => {
        const start = indexSource.indexOf('function triggerOcrPrewarm(');
        const end = indexSource.indexOf('function exitAnnotationMode()', start);
        const body = indexSource.slice(start, end);
        const activeOn = body.indexOf('ss.ocrPrewarmActive = true;');
        const activeOff = body.indexOf('ss.ocrPrewarmActive = false;');
        assert.ok(activeOn >= 0, '预热请求发起时应点亮');
        assert.ok(activeOff > activeOn, '预热 settle/失败时应熄灭');
        // 熄灭必须在 finally 的 handle 匹配守卫内（旧 handle 不熄灭新预热）
        const guard = body.lastIndexOf('if (ss.activeOcrHandle === handle)', activeOff);
        assert.ok(guard >= 0 && guard < activeOff, '熄灭应受 activeOcrHandle === handle 守卫');
        const sync = body.indexOf('updateOcrButtonBusy();', activeOff);
        assert.ok(sync > activeOff, '熄灭后应同步动效状态');
    });
});
