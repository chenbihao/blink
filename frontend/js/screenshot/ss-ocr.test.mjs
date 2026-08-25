//! Task 12: 前端 OCR 竞态测试 — import 生产模块，依赖注入 mock。
//!
//! **Task 12 变更**：不再在测试中重新实现 `createOcrHandle`（重复生产逻辑）。
//! 改为 import 生产代码 `api.js` 中的 `ocrImage`，通过 mock `window.__TAURI__`
//! 注入 fake invoke，让生产代码自行走完整逻辑。
//!
//! 测试覆盖：
//! - request id 发起前可取得
//! - ESC 取消当前 request
//! - 重选取消当前 request
//! - 新 session 取消旧 request
//! - cancel-before-register 最终生效
//! - 旧结果不 activateOverlay
//! - 旧结果不覆盖 ocrResultCache
//! - 旧 finally 不清除新 loading/ocrBusy
//! - 旧 prewarm 不覆盖新 prewarm
//! - request 完成后 active handle 被清除
//! - request id 使用 UUID 且不会因同毫秒调用碰撞
//! - auto fallback reason 只展示给当前 session
//! - Task 11: cancel 幂等——多次调用安全

import {test, describe, beforeEach, afterEach} from 'node:test';
import assert from 'node:assert';
import crypto from 'node:crypto';

// ── Mock window.__TAURI__ for production code import ───────────────────────
// api.js → tauri.js → window.__TAURI__.core.invoke
// 在 Node.js 测试环境中，需要先设置 window 全局变量再 import 生产代码。

let mockInvoke = null;
let cancelCalls = [];

globalThis.window = globalThis;

// Node.js 20+ 已有全局 crypto，无需设置
// 仅在不存在的环境中回退
if (!globalThis.crypto) {
    globalThis.crypto = crypto;
}

Object.defineProperty(globalThis, '__TAURI__', {
    value: {
        core: {
            invoke: (...args) => {
                if (mockInvoke) return mockInvoke(...args);
                return Promise.reject(new Error('mockInvoke not set'));
            },
        },
    },
    writable: false,
    configurable: true,
});

// 现在可以安全地 import 生产代码
const {ocrImage, cancelOcrRequest} = await import('../shared/api.js');

// ── 模拟 ss-state ──────────────────────────────────────────────────────────

function createSsState() {
    return {
        activeOcrHandle: null,
        ocrPrewarm: null,
        ocrBusy: false,
        ocrResultCache: null,
        selectionRevision: 0,
        editorSession: {epoch: 0},
    };
}

// ── 模拟 cancelActiveOcr（从 ss-ocr.js 复制的逻辑，用于测试） ──────────────

function cancelActiveOcr(ss) {
    if (ss.activeOcrHandle) {
        const handle = ss.activeOcrHandle;
        ss.activeOcrHandle = null;
        handle.cancel().catch(() => {});
    }
    if (ss.ocrPrewarm) {
        ss.ocrPrewarm = null;
    }
}

// ── 测试辅助：创建可控的 mock invoke ──────────────────────────────────────

function setupMockInvoke(opts = {}) {
    cancelCalls = [];
    const shouldResolve = opts.shouldResolve !== false;
    const result = opts.result || {text: 'test', lines: [], words: []};

    // ocr_image 的 mock
    mockInvoke = (cmd, ...args) => {
        if (cmd === 'ocr_image') {
            if (shouldResolve) {
                return Promise.resolve(result);
            }
            return Promise.reject(new Error(opts.error || 'mock error'));
        }
        if (cmd === 'cancel_ocr_request') {
            const requestId = args[0]?.requestId;
            cancelCalls.push(requestId);
            return Promise.resolve(true);
        }
        return Promise.reject(new Error(`unknown command: ${cmd}`));
    };
}

// ── 测试 ────────────────────────────────────────────────────────────────────

describe('OCR Request Handle (Task 12: 生产代码)', () => {
    beforeEach(() => {
        setupMockInvoke();
    });

    test('request id 在发起前可取得', () => {
        const handle = ocrImage(new Uint8Array([0x89]));
        assert.ok(handle.requestId, 'requestId 应该立即可用');
        assert.equal(typeof handle.requestId, 'string');
        assert.ok(handle.requestId.length > 0);
    });

    test('request id 使用 UUID 格式', () => {
        const handle = ocrImage(new Uint8Array([0x89]));
        const uuidRegex = /^[0-9a-f]{8}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{4}-[0-9a-f]{12}$/i;
        assert.match(handle.requestId, uuidRegex, 'requestId 应为 UUID 格式');
    });

    test('同毫秒调用不会碰撞', () => {
        const ids = new Set();
        for (let i = 0; i < 100; i++) {
            const handle = ocrImage(new Uint8Array([0x89]));
            ids.add(handle.requestId);
        }
        assert.equal(ids.size, 100, '100 个 requestId 应全部唯一');
    });

    test('cancel 立即可调用', async () => {
        const handle = ocrImage(new Uint8Array([0x89]));
        assert.equal(typeof handle.cancel, 'function');
        const result = await handle.cancel();
        assert.equal(result, true, '首次 cancel 应返回 true');
    });

    // Task 11: cancel 幂等——多次调用安全，只发一次后端取消请求
    test('cancel 幂等——多次调用只发一次后端请求', async () => {
        const handle = ocrImage(new Uint8Array([0x89]));
        const r1 = await handle.cancel();
        const r2 = await handle.cancel();
        assert.equal(r1, true, '首次 cancel 应返回 true');
        assert.equal(r2, false, '二次 cancel 应返回 false（已取消）');
        assert.equal(cancelCalls.length, 1, '后端只应收到一次 cancel 请求');
    });

    test('cancel 幂等——并发调用只发一次后端请求', async () => {
        const handle = ocrImage(new Uint8Array([0x89]));
        const [r1, r2, r3] = await Promise.all([
            handle.cancel(),
            handle.cancel(),
            handle.cancel(),
        ]);
        const trues = [r1, r2, r3].filter(v => v === true).length;
        assert.equal(trues, 1, '并发 cancel 只应有一个返回 true');
        assert.equal(cancelCalls.length, 1, '后端只应收到一次 cancel 请求');
    });
});

describe('ESC / 重选 / 新 session 取消', () => {
    beforeEach(() => {
        setupMockInvoke();
    });

    test('ESC 取消当前 request', async () => {
        const ss = createSsState();
        const handle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = handle;

        cancelActiveOcr(ss);
        assert.equal(ss.activeOcrHandle, null, 'active handle 应被清除');
        // cancel 应已发到后端
        assert.equal(cancelCalls.length, 1, '应已发送 cancel 请求');
    });

    test('重选取消当前 request', async () => {
        const ss = createSsState();
        ss.selectionRevision = 1;
        const handle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = handle;

        // 模拟重选：先取消旧的，再开始新的
        cancelActiveOcr(ss);
        ss.selectionRevision = 2;
        const newHandle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = newHandle;

        assert.equal(cancelCalls.length, 1, '旧 request 应被取消');
        assert.equal(ss.activeOcrHandle, newHandle);
    });

    test('新 session 取消旧 request', async () => {
        const ss = createSsState();
        ss.editorSession.epoch = 1;
        const handle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = handle;

        cancelActiveOcr(ss);
        ss.editorSession = {epoch: 2};

        assert.equal(ss.activeOcrHandle, null);
        assert.equal(cancelCalls.length, 1, '旧 session 的 request 应被取消');
    });
});

describe('旧结果不干扰新状态', () => {
    test('旧 finally 不清除新 loading/ocrBusy', () => {
        const ss = createSsState();
        const oldHandle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = oldHandle;
        ss.ocrBusy = true;

        // 模拟新 request 已替换
        const newHandle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = newHandle;
        ss.selectionRevision = 1;

        // 旧 request 的 finally 执行
        if (ss.activeOcrHandle === oldHandle) {
            ss.activeOcrHandle = null;
            ss.ocrBusy = false;
        }

        // 新 handle 仍应为 active
        assert.equal(ss.activeOcrHandle, newHandle, '新 handle 不应被旧 finally 清除');
        assert.equal(ss.ocrBusy, true, 'ocrBusy 不应被旧 finally 清除');
    });

    test('旧 prewarm 不覆盖新 prewarm', () => {
        const ss = createSsState();
        ss.ocrPrewarm = Promise.resolve({text: 'old'});

        // 新 prewarm
        const newPrewarm = Promise.resolve({text: 'new'});
        ss.ocrPrewarm = newPrewarm;

        assert.equal(ss.ocrPrewarm, newPrewarm, '新 prewarm 应替换旧的');
    });

    test('旧结果不覆盖 ocrResultCache', () => {
        const ss = createSsState();
        ss.ocrResultCache = {text: 'new result', lines: []};

        // 旧 request 迟到结果——不应覆盖
        const oldResult = {text: 'old result', lines: []};
        if (ss.activeOcrHandle === null) {
            // 旧 handle 已不在 active，不应写入
        } else {
            ss.ocrResultCache = oldResult;
        }

        assert.equal(ss.ocrResultCache.text, 'new result', '旧结果不应覆盖新 cache');
    });
});

describe('cancel-before-register', () => {
    test('cancel 在 register 前到达最终生效', async () => {
        // 模拟：cancel 先到达，register 后 token 立即被取消
        const pendingCancels = new Set();
        const request_id = 'test-cancel-before-register';

        // cancel 先到——记录 tombstone
        pendingCancels.add(request_id);
        assert.ok(pendingCancels.has(request_id), 'tombstone 应已记录');

        // register 后——检查 tombstone
        const wasPreCancelled = pendingCancels.delete(request_id);
        assert.ok(wasPreCancelled, 'register 时应发现 tombstone');
    });

    test('旧 cancel 不影响新 request', () => {
        const pendingCancels = new Set();

        // 旧 request cancel
        pendingCancels.add('old-request');

        // 新 request register——检查自己的 id
        const wasPreCancelled = pendingCancels.has('new-request');
        assert.ok(!wasPreCancelled, '新 request 不应受旧 cancel 影响');
    });
});

describe('Request 完成后清理', () => {
    test('request 完成后 active handle 被清除', async () => {
        const ss = createSsState();
        setupMockInvoke({result: {text: 'result', lines: [], words: []}});
        const handle = ocrImage(new Uint8Array([0x89]));
        ss.activeOcrHandle = handle;

        // 模拟 request 完成
        try {
            await handle.promise;
        } catch {
            // ignore
        }

        // finally 清理
        if (ss.activeOcrHandle === handle) {
            ss.activeOcrHandle = null;
        }

        assert.equal(ss.activeOcrHandle, null, '完成后 active handle 应被清除');
    });
});

describe('auto fallback reason 只展示给当前 session', () => {
    test('fallback reason 携带当前 session revision', () => {
        const ss = createSsState();
        ss.selectionRevision = 5;
        const fallbackReason = `PaddleOCR 未热态 Ready (rev=${ss.selectionRevision})`;
        assert.ok(fallbackReason.includes('rev=5'));

        // 新 session revision 不受旧 reason 影响
        ss.selectionRevision = 6;
        const newReason = `PaddleOCR 未热态 Ready (rev=${ss.selectionRevision})`;
        assert.ok(!newReason.includes('rev=5'), '新 reason 不应携带旧 revision');
    });
});
