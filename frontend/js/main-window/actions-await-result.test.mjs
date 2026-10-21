//! 0.26.20 修订：慢 action 通用协议（descriptor 声明 ReturnItems → Action
//! awaitResult=true）——不再按 id 前端特判（原 check_update 直调专用命令已删）。
//! - Enter（awaitResult）→ 先占位（title=t(hint)、副行=t(action.please_wait)），
//!   占位先于 IPC 出现（用户零等待看到转圈）
//! - run_builtin_action 返回 {items} → settleActionItems 原样透传（后端构造
//!   标准条目，含本地化文案与 icon），主窗不隐藏
//! - 返回 {done} → 不 settle（后端已隐藏窗口，现状收尾）
//! - invoke 抛错 → settleActionItems 通用错误行兜底（占位不得悬挂）
//! - settings kind（结果行携带的纯 UI 导航）→ openSettingsTab + hideWindow
//! 用 results-viewport.test.mjs 同款 vm 方案：剥 import，mock 结果区/IPC 边界。

import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import {test} from 'node:test';
import vm from 'node:vm';

const source = readFileSync(new URL('./actions.js', import.meta.url), 'utf-8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace(/^export /gm, '');

function setup() {
    const pending = [];   // showActionPending(title, hint) 调用
    const settled = [];   // settleActionItems(items) 调用
    const calls = [];     // IPC / 窗口调用轨迹
    let builtinResult = {items: [{name: '已是最新版本（0.26.20）', source: 'update', icon: 'check'}]};
    let invokeThrows = false;
    const context = vm.createContext({
        runBuiltinAction: async (id, arg) => {
            calls.push(['run_builtin_action', id, arg]);
            if (invokeThrows) throw new Error('ipc down');
            return builtinResult;
        },
        openSettingsTab: async (tab) => calls.push(['open_settings_tab', tab]),
        hideWindow: async () => calls.push(['hide_window']),
        copyToClipboard: async () => {},
        results: {
            showActionPending: (title, hint) => pending.push([title, hint]),
            settleActionItems: (items) => settled.push(items),
        },
        showActionError: (where) => calls.push(['action_error', where]),
        t: (key, params) => key + (params ? ':' + JSON.stringify(params) : ''),
        queryEl: {value: '', setSelectionRange() {}, dispatchEvent() {}},
        console, Promise, Math, Set, Map, JSON, Array, Object, Number, String,
    });
    vm.runInContext(source, context);
    return {
        context, pending, settled, calls,
        runAwait() {
            return context.activateItem({actions: [{kind: 'run', runId: 'check_update', awaitResult: true, hint: 'update.checking'}]});
        },
        runPlain() {
            return context.activateItem({actions: [{kind: 'run', runId: 'open_settings', runArg: null}]});
        },
        set result(v) { builtinResult = v; },
        set throws(v) { invokeThrows = v; },
    };
}

test('awaitResult：Enter 即占位（t(hint) 主行 + 通用请稍候副行），先于 IPC', async () => {
    const h = setup();
    const p = h.runAwait();
    assert.deepEqual(h.pending, [['update.checking', 'action.please_wait']], '占位在同步段已展示');
    await p;
    assert.equal(h.calls.filter((c) => c[0] === 'run_builtin_action').length, 1, '随后 invoke 一次');
});

test('返回 {items}：后端条目原样回流（settleActionItems 透传），主窗不隐藏', async () => {
    const h = setup();
    const items = [
        {name: '发现新版本 0.26.21', source: 'update', icon: 'download',
            actions: [{kind: 'settings', payload: 'about'}]},
    ];
    h.result = {items};
    await h.runAwait();
    assert.deepEqual(h.settled, [items], '条目零加工透传（文案/图标/动作都在后端）');
    assert.ok(!h.calls.some((c) => c[0] === 'hide_window'), '主窗不隐藏');
});

test('返回 {done}：不 settle——后端已隐藏窗口（现状收尾不变）', async () => {
    const h = setup();
    h.result = {done: true};
    await h.runAwait();
    assert.equal(h.settled.length, 0);
});

test('invoke 抛错：settle 通用错误行兜底，占位不得悬挂', async () => {
    const h = setup();
    h.throws = true;
    await h.runAwait();
    // 错误行对象在 vm realm 内构造，deepEqual 跨 realm 原型不等——逐字段断言
    assert.equal(h.settled.length, 1);
    assert.equal(h.settled[0].length, 1);
    assert.equal(h.settled[0][0].name, 'action.failed');
    assert.equal(h.settled[0][0].is_error, true);
    assert.ok(!h.calls.some((c) => c[0] === 'action_error'), 'awaitResult 失败不走 statusbar');
});

test('普通 run（无 awaitResult）：不占位，失败走 statusbar（现状不变）', async () => {
    const h = setup();
    h.throws = true;
    await h.runPlain();
    assert.equal(h.pending.length, 0, '无占位');
    assert.equal(h.settled.length, 0);
    assert.ok(h.calls.some((c) => c[0] === 'action_error' && c[1] === 'run_builtin_action'));
});

test('settings kind：openSettingsTab(payload) + 隐藏主窗', async () => {
    const h = setup();
    await h.context.activateItem({actions: [{kind: 'settings', payload: 'about'}]});
    assert.ok(h.calls.some((c) => c[0] === 'open_settings_tab' && c[1] === 'about'));
    assert.ok(h.calls.some((c) => c[0] === 'hide_window'));
});

test('check_update 不再前端特判：无 awaitResult 标志（旧形状）不占位', async () => {
    const h = setup();
    await h.context.activateItem({actions: [{kind: 'run', runId: 'check_update'}]});
    assert.equal(h.pending.length, 0, '占位由 descriptor 声明驱动，非 id 硬编码');
});
