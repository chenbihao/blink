//! 0.26.10 右键智能操作（打开类条目派生）的菜单会话回归：
//! loading 先行、异步回流更新同一菜单、内容代次拒绝旧点击、竞态不覆盖新会话。
import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as entryModel from './clipboard-entry.js';

const source = readFileSync(new URL('./contextmenu.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace('import("../shared/tauri.js")', 'Promise.resolve({listen})')
    .replace(/^export /gm, '');

const textActions = [
    {kind: 'copy', hint: 'menu.copy', hitId: 't1'},
    {kind: 'run', runId: 'edit_text_item', hint: 'menu.edit', runArg: {originRef: 't1'}},
];
// 该 fixture 同时由 Rust 测试比对真实派生结果的 serde wire 形态。
const rustActions = JSON.parse(readFileSync(new URL('./fixtures/clipboard-item-actions.json', import.meta.url), 'utf8'));

function setup({refs = [{kind: 'text', id: 't1'}], itemActions = null} = {}) {
    let currentRefs = refs;
    let selectionRevision = 1;
    const events = new Map(), ipc = [], calls = [], telemetry = [];
    let pendingResolve;
    const clipboardMode = {
        isActive: () => true,
        hasSelection: () => currentRefs.length > 0,
        prepareContextMenu(li) {
            const ref = entryModel.refFromLi(li);
            if (ref && !currentRefs.some(current => entryModel.refKey(current) === entryModel.refKey(ref))) {
                currentRefs = [ref]; selectionRevision++;
            }
        },
        getSelectionSnapshot: () => ({refs: currentRefs.map(ref => ({...ref})), keys: currentRefs.map(entryModel.refKey), epoch: 1, revision: selectionRevision}),
        clearSelection() {},
    };
    const context = vm.createContext({
        ...entryModel, clipboardMode,
        EVENTS: {CONTEXT_MENU_ACTION: 'action', CONTEXT_MENU_CLOSED: 'closed', SHOWN: 'shown', HIDDEN: 'hidden'},
        getClipboardPreviewContext: () => null,
        holdClipboardPreview: () => () => {},
        document: {addEventListener: (name, callback) => events.set(name, callback)},
        listen: (name, callback) => events.set(name, callback),
        invoke: async (command, args) => {
            ipc.push({command, args});
            if (command === 'get_clipboard_item_actions') {
                if (itemActions instanceof Error) throw itemActions;
                if (typeof itemActions === 'function') return itemActions(args);
                return itemActions ? structuredClone(itemActions) : [];
            }
            if (command === 'report_clipboard_item_action') telemetry.push(args);
        },
        runBuiltinAction: async (id, arg) => calls.push({id: 'run', capability: id, arg}),
        t: (key, values) => key + (values?.text !== undefined ? ':' + values.text : values?.count ? ':' + values.count : ''),
        getLang: () => 'zh', parseColor: () => null, resultsEl: {contains: () => true}, queryEl: {},
        activateItem: data => calls.push({id: 'activate', data}),
        showActionError: () => calls.push({id: 'error'}),
        hideWindow: () => {},
        console, Promise, Math, Date, JSON,
    });
    vm.runInContext(source, context);
    context.init();
    pendingResolve = Promise.resolve();
    const menus = () => ipc.filter(call => call.command === 'show_context_menu')
        .map(call => JSON.parse(call.args.items));
    const openSingle = () => {
        // 单项右键（无多选）：把 refs 置空让 hasSelection() 为 false
        currentRefs = [];
        const ref = refs[0];
        const li = {dataset: {source: 'clipboard', clipboardKind: ref.kind, clipboardId: ref.id,
            actions: JSON.stringify(textActions)}, closest() {return this;}, querySelector: () => null, isConnected: true};
        events.get('contextmenu')({target: li, preventDefault() {}});
        return li;
    };
    return {
        events, ipc, calls, telemetry, menus, openSingle,
        set refs(value) {currentRefs = value; selectionRevision++;},
        get refs() {return currentRefs;},
        flush: async () => {await pendingResolve; await new Promise(resolve => setImmediate(resolve));},
    };
}

test('单项文本菜单顶部先显示 loading，结果回流后更新同一菜单', async () => {
    const h = setup({itemActions: [
        {kind: 'openUrl', capabilityId: 'open_url', args: {url: 'https://example.com'}, target: 'https://example.com', actionKey: 'k1'},
        {kind: 'revealInExplorer', capabilityId: 'reveal_in_explorer', args: {path: 'C:\\a'}, target: 'C:\\a', actionKey: 'k2'},
    ]});
    h.openSingle();
    const initial = h.menus().at(-1);
    assert.equal(initial[0].label, 'menu.smart_loading');
    assert.equal(initial[0].disabled, true);
    assert.ok(initial[0].menuId, 'loading 项已绑定会话');
    assert.equal(initial[0].generation, 0, '初始内容也携带代次');
    assert.equal(h.refs.length, 1, '真实右键准备已选中单项');
    assert.equal(initial.find(item => item.label === 'menu.copy').menuId, initial[0].menuId);
    await h.flush();
    const updated = h.menus().at(-1);
    assert.equal(updated[0].label, 'suggestionbar.openUrl_with:https://example.com');
    assert.equal(updated[0].icon, 'globe');
    assert.equal(updated[0].generation, 1, '智能组更新推进内容代次');
    assert.equal(updated[1].label, 'suggestionbar.revealInExplorer_with:C:\\a');
    assert.equal(updated[1].icon, 'folder-search');
    // 基础菜单保留且同步代次
    assert.equal(updated.find(item => item.label === 'menu.copy').generation, 1);
});

test('空组移除 loading 分组，还原纯基础菜单', async () => {
    const h = setup({itemActions: []});
    h.openSingle();
    await h.flush();
    const updated = h.menus().at(-1);
    assert.ok(!updated.some(item => item.label === 'menu.smart_loading'), '空组移除 loading');
    assert.ok(updated.some(item => item.label === 'menu.copy'), '基础操作保留');
});

test('识别失败显示不可点击失败态，基础菜单继续可用', async () => {
    const h = setup({itemActions: new Error('db down')});
    h.openSingle();
    await h.flush();
    const updated = h.menus().at(-1);
    assert.equal(updated[0].label, 'menu.smart_failed');
    assert.equal(updated[0].disabled, true);
    assert.ok(updated.some(item => item.label === 'menu.copy'));
});

test('点击智能项经 run_builtin_action 执行并上报遥测', async () => {
    const h = setup({itemActions: rustActions});
    h.openSingle();
    await h.flush();
    const updated = h.menus().at(-1);
    h.events.get('action')({payload: {actionId: 0, menuId: updated[0].menuId, generation: updated[0].generation}});
    await h.flush();
    assert.equal(h.calls[0].capability, 'open_url');
    // vm 上下文对象原型与主上下文不同，JSON 化后再比较
    assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].arg)), {url: 'https://example.com/page'});
    assert.deepEqual(JSON.parse(JSON.stringify(h.telemetry.at(-1))), {kind: 'openUrl', capabilityId: 'open_url', outcome: 'ok'});
});

test('旧代次点击被拒绝，不按新数组位置执行另一操作', async () => {
    const h = setup({itemActions: [
        {kind: 'openUrl', capabilityId: 'open_url', args: {url: 'https://a'}, target: 'https://a', actionKey: 'k'},
    ]});
    h.openSingle();
    const initial = h.menus().at(-1);
    await h.flush();
    const updated = h.menus().at(-1);
    // 旧数字代次、空代次、缺失代次都不能按更新后的下标执行。
    for (const generation of [initial[0].generation, null, undefined]) {
        h.events.get('action')({payload: {actionId: 0, menuId: updated[0].menuId, generation}});
    }
    assert.equal(h.calls.length, 0, '旧代次点击不执行新位置的动作');
    // 新代次点击正常
    h.events.get('action')({payload: {actionId: 0, menuId: updated[0].menuId, generation: updated[0].generation}});
    await h.flush();
    assert.equal(h.calls[0].capability, 'open_url');
});

test('菜单切换条目后，旧查询结果不更新新会话', async () => {
    let resolveFirst;
    const events = new Map(), ipc = [], calls = [];
    const refs = [{kind: 'text', id: 't1'}];
    let currentRefs = [];
    const clipboardMode = {
        isActive: () => true, hasSelection: () => currentRefs.length > 0, prepareContextMenu() {},
        getSelectionSnapshot: () => ({refs: [], keys: [], epoch: 1, revision: 1}), clearSelection() {},
    };
    const context = vm.createContext({
        ...entryModel, clipboardMode,
        EVENTS: {CONTEXT_MENU_ACTION: 'a', CONTEXT_MENU_CLOSED: 'c', SHOWN: 's', HIDDEN: 'h'},
        getClipboardPreviewContext: () => null, holdClipboardPreview: () => () => {},
        document: {addEventListener: (n, cb) => events.set(n, cb)},
        listen: (n, cb) => events.set(n, cb),
        invoke: async (command, args) => {
            ipc.push({command, args});
            if (command === 'get_clipboard_item_actions' && args?.entry?.id === 't1') {
                await new Promise(resolve => {resolveFirst = resolve;});
                return [];
            }
            return [];
        },
        runBuiltinAction: async () => {}, t: key => key, getLang: () => 'zh',
        parseColor: () => null, resultsEl: {contains: () => true}, queryEl: {},
        activateItem: () => {}, showActionError: () => {}, hideWindow: () => {},
        console, Promise, Math, Date, JSON,
    });
    vm.runInContext(source, context);
    context.init();
    const li1 = {dataset: {source: 'clipboard', clipboardKind: 'text', clipboardId: 't1',
        actions: '[]'}, closest() {return this;}, querySelector: () => null, isConnected: true};
    events.get('contextmenu')({target: li1, preventDefault() {}});
    const li2 = {dataset: {source: 'clipboard', clipboardKind: 'text', clipboardId: 't2',
        actions: '[]'}, closest() {return this;}, querySelector: () => null, isConnected: true};
    events.get('contextmenu')({target: li2, preventDefault() {}});
    resolveFirst();
    await new Promise(resolve => setImmediate(resolve));
    const shows = ipc.filter(call => call.command === 'show_context_menu').map(call => JSON.parse(call.args.items));
    // t1 初始 + t2 初始 + t2 自身空组还原 = 3 次；t1 的迟到结果不得产生任何后续更新
    assert.equal(shows.length, 3, '旧会话结果不重开/不更新新菜单');
    const t1MenuId = shows[0][0].menuId;
    const t2MenuId = shows[1][0].menuId;
    assert.notEqual(t1MenuId, t2MenuId);
    assert.ok(shows.at(-1).every(item => item.menuId === t2MenuId),
        '最终菜单只属于 t2 会话（t1 的 loading 项不回流）');
    assert.ok(!shows.at(-1).some(item => item.label === 'menu.smart_loading'),
        't2 空组还原为基础菜单');
});

test('多选接管与图片条目不注入智能组', async () => {
    const h = setup({refs: [{kind: 'text', id: 'a'}, {kind: 'text', id: 'b'}], itemActions: []});
    // 多选（refs 2 条 → hasSelection true）：批量菜单接管，无 loading
    const li = {dataset: {source: 'clipboard', clipboardKind: 'text', clipboardId: 'a',
        actions: JSON.stringify(textActions)}, closest() {return this;}, querySelector: () => null};
    h.events.get('contextmenu')({target: li, preventDefault() {}});
    assert.ok(!h.menus().at(-1).some(item => item.label === 'menu.smart_loading'), '多选不注入');
    assert.ok(!h.ipc.some(call => call.command === 'get_clipboard_item_actions'));
});

test('初始编辑点击与双动作组更新竞争时不误执行复制', async () => {
    const h = setup({itemActions: [
        {kind: 'openPath', capabilityId: 'open_path', args: {path: 'C:\\dir'}, target: 'C:\\dir'},
        {kind: 'revealInExplorer', capabilityId: 'reveal_in_explorer', args: {path: 'C:\\dir'}, target: 'C:\\dir'},
    ]});
    h.openSingle();
    const initial = h.menus().at(-1), edit = initial.findIndex(item => item.label === 'menu.edit');
    await h.flush();
    const updated = h.menus().at(-1);
    assert.equal(updated[edit].label, 'menu.copy', '插入动作使旧下标指向另一操作');
    for (const generation of [initial[edit].generation, null, undefined]) {
        h.events.get('action')({payload: {actionId: edit, menuId: initial[edit].menuId, generation}});
    }
    assert.equal(h.calls.length, 0);
    const currentEdit = updated.findIndex(item => item.label === 'menu.edit');
    h.events.get('action')({payload: {actionId: currentEdit, menuId: updated[currentEdit].menuId, generation: updated[currentEdit].generation}});
    assert.equal(h.calls[0].data.actions[0].runId, 'edit_text_item');
});

test('查询回流及点击均拒绝已改变的选择快照', async () => {
    let finish;
    const pending = setup({itemActions: () => new Promise(resolve => {finish = resolve;})});
    pending.openSingle();
    pending.refs = [{kind: 'text', id: 'new'}];
    finish(rustActions);
    await pending.flush();
    assert.equal(pending.menus().length, 1, '锚点仍连接也不能回写旧选择');

    const clicked = setup({itemActions: rustActions});
    clicked.openSingle(); await clicked.flush();
    const menu = clicked.menus().at(-1);
    clicked.refs = [{kind: 'text', id: 'new'}];
    clicked.events.get('action')({payload: {actionId: 0, menuId: menu[0].menuId, generation: menu[0].generation}});
    assert.equal(clicked.calls.length, 0, '派生完成后改变选择也不能执行旧目标');
});
