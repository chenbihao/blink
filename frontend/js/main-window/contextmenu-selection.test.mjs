import {test} from 'node:test';
import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as entryModel from './clipboard-entry.js';

const source = readFileSync(new URL('./contextmenu.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace('import("../shared/tauri.js")', 'Promise.resolve({listen})')
    .replace(/^export /gm, '');

async function setup(initialRefs, overrides = {}) {
    let refs = initialRefs, revision = 1;
    const events = new Map(), ipc = [], calls = [];
    const clipboardMode = {
        isActive: () => true, hasSelection: () => refs.length > 0, prepareContextMenu() {},
        getSelectionSnapshot: () => ({refs: refs.map(ref => ({...ref})), keys: refs.map(entryModel.refKey), epoch: 1, revision}),
        clearSelection: task => calls.push({id: 'clear', task}),
    };
    for (const [name, id] of Object.entries({batchCopy: 'copy', batchPaste: 'paste', batchEdit: 'edit', batchSticky: 'sticky', batchDelete: 'delete', batchPin: 'pin'})) {
        clipboardMode[name] = task => calls.push({id, task});
    }
    const context = vm.createContext({
        ...entryModel, clipboardMode, EVENTS: {CONTEXT_MENU_ACTION: 'action', SHOWN: 'shown', HIDDEN: 'hidden'},
        document: {addEventListener: (name, callback) => events.set(name, callback)},
        listen: (name, callback) => events.set(name, callback),
        invoke: async (command, args) => {ipc.push({command, args});},
        t: (key, values) => key + (values?.count ? ':' + values.count : ''),
        getLang: () => 'zh', parseColor: () => null, resultsEl: {contains: () => true}, queryEl: {},
        activateItem: data => calls.push({id: 'activate', data}),
        hideWindow: () => calls.push({id: 'hide'}),
        console, Promise, Math, Date, JSON,
        ...overrides,
    });
    vm.runInContext(source, context); context.init(); await Promise.resolve();
    return {
        calls, events, ipc, clipboardMode,
        set refs(value) {refs = value; revision++;},
        open(actions = []) {
            const ref = refs[0];
            const li = {dataset: {source: 'clipboard', clipboardKind: ref.kind, clipboardId: ref.id, actions: JSON.stringify(actions)},
                closest() {return this;}, querySelector: () => null};
            events.get('contextmenu')({target: li, preventDefault() {}});
            return JSON.parse(ipc.filter(call => call.command === 'show_context_menu').at(-1).args.items);
        },
    };
}

test('错误行与悬浮全文的右键菜单复制完整快照，列表刷新不改变复制内容', async () => {
    const copied = [];
    const h = await setup([], {copyToClipboard:async text => copied.push(text)});
    const text = '插件返回错误 (invalid_arguments): ' + '很长的错误内容\n'.repeat(80);
    for (const popup of [false,true]) {
        const li = {dataset:{isError:'true',errorText:text,actions:'[]'},querySelector:()=>null};
        const preview = {dataset:{errorText:text}};
        const target = {closest:selector => selector === 'li' && !popup ? li
            : selector === '.clipboard-text-preview' && popup ? preview : null};
        h.events.get('contextmenu')({target,preventDefault() {}});
        const menu = JSON.parse(h.ipc.filter(call => call.command === 'show_context_menu').at(-1).args.items);
        assert.equal(menu.length,1);
        assert.equal(menu[0].label,'menu.copyError');
        li.dataset.errorText = 'new error';
        preview.dataset.errorText = '';
        h.events.get('action')({payload:{actionId:0,menuId:menu[0].menuId}});
        await Promise.resolve();
        assert.equal(copied.at(-1),text);
    }
});

test('旧菜单回调不会按新菜单的相同数组下标执行', async () => {
    const h = await setup([{kind: 'text', id: 'a'}, {kind: 'image', id: 'b'}]);
    const old = h.open();
    h.refs = [{kind: 'image', id: 'new'}];
    const latest = h.open();
    h.events.get('action')({payload: {actionId: 0, menuId: old[0].menuId}});
    assert.equal(h.calls.length, 0);
    h.events.get('action')({payload: {actionId: 0, menuId: latest[0].menuId}});
    assert.equal(h.calls[0].id, 'pin');
    assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].task.refs)), [{kind: 'image', id: 'new'}]);
    h.events.get('action')({payload: {actionId: 0, menuId: latest[0].menuId}});
    assert.equal(h.calls.length, 1, '同一次菜单选择只消费一次');
});
test('菜单文案计数与执行快照来自同一组目标', async () => {
    const h = await setup([{kind: 'text', id: 'a'}, {kind: 'image', id: 'b'}]);
    const menu = h.open();
    assert.equal(menu[0].label, 'menu.selection_copy:1');
    assert.equal(menu[0].shortcut, '');
    h.refs = [{kind: 'text', id: 'different'}];
    h.events.get('action')({payload: {actionId: 0, menuId: menu[0].menuId}});
    assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].task.refs)), [{kind: 'text', id: 'a'}]);
});
test('混选菜单固定先文本后图片，删除全部置后，点击顺序不改变菜单及目标', async () => {
    const refs = [{kind: 'image', id: 'i1'}, {kind: 'text', id: 't1'},
        {kind: 'image', id: 'i2'}, {kind: 'text', id: 't2'}];
    for (const order of [refs, refs.slice().reverse()]) {
        const h = await setup(order);
        const menu = h.open();
        assert.deepEqual(menu.filter(item => !item.separator).map(item => item.label), [
            'menu.selection_copy:2', 'menu.selection_paste:2', 'menu.selection_edit:2',
            'menu.selection_sticky:2', 'menu.selection_pin:2',
            'menu.delete_scope_text:2', 'menu.delete_scope_image:2', 'menu.delete_scope_all:4',
            'menu.deselect',
        ]);
        for (const scope of ['text', 'image', 'all']) {
            const latest = h.open();
            const actionId = latest.findIndex(item => item.label?.startsWith('menu.delete_scope_' + scope + ':'));
            h.events.get('action')({payload: {actionId, menuId: latest[actionId].menuId}});
            const call = h.calls.at(-1);
            assert.equal(call.id, 'delete');
            assert.deepEqual(JSON.parse(JSON.stringify(call.task.refs)),
                scope === 'all' ? order : order.filter(ref => ref.kind === scope));
        }
    }
});
test('主窗隐藏后已展示的菜单回调失效', async () => {
    const h = await setup([{kind: 'text', id: 'a'}, {kind: 'image', id: 'b'}]);
    const menu = h.open(); h.events.get('hidden')();
    h.events.get('action')({payload: {actionId: 0, menuId: menu[0].menuId}});
    assert.equal(h.calls.length, 0);
});

test('单项文本/图片复制标注 Ctrl+C 并保留类型引用，普通搜索不误标', async () => {
    for (const [ref, action] of [
        [{kind: 'text', id: 'a'}, {kind: 'copy', hint: 'menu.copy'}],
        [{kind: 'image', id: 'b'}, {kind: 'run', runId: 'copy_clipboard_image', hint: 'menu.copy'}],
    ]) {
        const h = await setup([ref]);
        const menu = h.open([action]);
        assert.equal(menu[0].shortcut, 'Ctrl+C');
        h.events.get('action')({payload: {actionId: 0, menuId: menu[0].menuId}});
        assert.deepEqual(JSON.parse(JSON.stringify(h.calls[0].data.clipboardRef)), ref);
        h.clipboardMode.isActive = () => false;
        assert.equal(h.open([action])[0].shortcut, undefined, '普通搜索中 Ctrl+C 仍用于输入框原生复制');
    }
});

test('未声明操作的新类型单选只提供取消选择，不猜测删除能力', async () => {
    const h = await setup([{kind: 'audio', id: 'future'}]);
    const menu = h.open();
    assert.equal(menu.length, 1);
    assert.equal(menu[0].label, 'menu.deselect');
});

test('旧图片编辑完成不会隐藏后来的选择', async () => {
    let finish;
    const h = await setup([{kind: 'image', id: 'old'}], {
        openImageEditorFromHistory: () => new Promise(resolve => {finish = resolve;}),
    });
    const menu = h.open();
    const index = menu.findIndex(item => item.label === 'menu.editImage');
    h.events.get('action')({payload: {actionId: index, menuId: menu[index].menuId}});
    h.refs = [{kind: 'text', id: 'new'}];
    finish(); await Promise.resolve();
    assert.equal(h.calls.length, 0);
});
