import assert from 'node:assert/strict';
import {test} from 'node:test';

const nodes = new Map();
const listeners = new Map();
const calls = [];
let failStatus = false;
let backend = {epoch: 'service', revision: 0, runtime: {
    state: 'missing', targetVersion: '9.0.2', downloadBytes: 114768076, installedBytes: 0,
}, operation: null, lastError: null};
function node(id) {
    if (!nodes.has(id)) nodes.set(id, {
        hidden: false, disabled: false, textContent: '', className: '', style: {}, handlers: {},
        classList: {toggle() {}},
        addEventListener(name, handler) { this.handlers[name] = handler; },
        querySelector(selector) { return node(`${id}:${selector}`); },
    });
    return nodes.get(id);
}
globalThis.document = {
    hidden: false, getElementById: node, querySelector: () => ({}),
    addEventListener(name, handler) { listeners.set(name, handler); },
};
globalThis.window = {__TAURI__: {
    core: {invoke: async (command, args) => {
        calls.push({command, args});
        if (command === 'get_ffmpeg_runtime_status') {
            if (failStatus) throw {code: 'temporary', message: 'temporary status error'};
            return structuredClone(backend);
        }
        if (command === 'begin_ffmpeg_runtime_operation') {
            backend = {...backend, revision: backend.revision + 1, operation: {
                id: 'install-1', stage: 'downloading', cancellable: true, downloaded: 42, total: 114768076,
            }};
            return 'install-1';
        }
        return null;
    }},
    event: {listen: async (name, handler) => { listeners.set(name, handler); return () => {}; }},
}};
globalThis.setInterval = () => 0;
const {initMediaRuntime} = await import('./media-runtime.js');
const flush = () => new Promise(resolve => setImmediate(resolve));

test('card restores a backend operation, isolates Everything notifications, and cancels by operation id', async () => {
    initMediaRuntime();
    await flush();
    assert.equal(node('media-runtime-action').disabled, false);
    assert.equal(node('media-runtime-status').textContent, '未安装');
    await node('media-runtime-action').handlers.click();
    await flush();
    assert.deepEqual(calls.find(c => c.command === 'begin_ffmpeg_runtime_operation').args, {action: 'install'});
    assert.equal(node('media-runtime-action').disabled, true);
    assert.equal(node('media-runtime-cancel').hidden, false);
    const before = calls.length;
    listeners.get('blink://managed-runtime-install')({payload: {kind: 'everything', stage: 'done'}});
    await flush();
    assert.equal(calls.length, before, '另一运行时通知不能驱动此卡片');
    await node('media-runtime-cancel').handlers.click();
    assert.deepEqual(calls.find(c => c.command === 'cancel_ffmpeg_runtime_operation').args, {operationId: 'install-1'});
    backend = {...backend, revision: 2, runtime: {...backend.runtime, state: 'ready', version: '9.0.2', installedBytes: 210722981}, operation: null};
    listeners.get('visibilitychange')();
    await flush();
    assert.equal(node('media-runtime-action').textContent, '测试可用性');
    assert.equal(node('media-runtime-repair').hidden, false);
    assert.equal(node('media-runtime-progress').hidden, true);
    assert.equal(node('media-runtime-cancel').hidden, true);
    backend = {...backend, revision: 3, runtime: {...backend.runtime, state: 'corrupt'}, lastError: '测试失败'};
    listeners.get('blink://managed-runtime-install')({payload: {kind: 'ffmpeg', operationId: 'old-notification'}});
    await flush();
    assert.equal(node('media-runtime-action').textContent, '修复 / 更新');
    assert.equal(node('media-runtime-error').textContent, '测试失败');
    const count = calls.length;
    initMediaRuntime();
    await flush();
    assert.equal(calls.length, count, '重复初始化不重复注册/查询');
    backend = {...backend, lastError: null};
    failStatus = true;
    listeners.get('visibilitychange')();
    await flush();
    assert.equal(node('media-runtime-error').textContent, 'temporary status error');
    failStatus = false;
    listeners.get('visibilitychange')();
    await flush();
    assert.equal(node('media-runtime-error').hidden, true, '成功重查应清除瞬态查询错误');
});
