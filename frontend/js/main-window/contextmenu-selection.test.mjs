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
    const events = new Map(), ipc = [], calls = [], prepared = [], holds = [], releases = [];
    const clipboardMode = {
        isActive: () => true, hasSelection: () => refs.length > 0, prepareContextMenu(li) {prepared.push(li);},
        getSelectionSnapshot: () => ({refs: refs.map(ref => ({...ref})), keys: refs.map(entryModel.refKey), epoch: 1, revision}),
        clearSelection: task => calls.push({id: 'clear', task}),
    };
    for (const [name, id] of Object.entries({batchCopy: 'copy', batchPaste: 'paste', batchEdit: 'edit', batchSticky: 'sticky', batchDelete: 'delete', batchPin: 'pin'})) {
        clipboardMode[name] = task => calls.push({id, task});
    }
    const context = vm.createContext({
        ...entryModel, clipboardMode, EVENTS: {CONTEXT_MENU_ACTION: 'action', CONTEXT_MENU_CLOSED: 'closed', SHOWN: 'shown', HIDDEN: 'hidden'},
        getClipboardPreviewContext: target => target.previewContext ?? null,
        holdClipboardPreview: context => {holds.push(context); return () => releases.push(context);},
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
        calls, events, ipc, clipboardMode, prepared, holds, releases,
        set refs(value) {refs = value; revision++;},
        open(actions = []) {
            const ref = refs[0];
            const li = {dataset: {source: 'clipboard', clipboardKind: ref.kind, clipboardId: ref.id, actions: JSON.stringify(actions)},
                closest() {return this;}, querySelector: () => null};
            events.get('contextmenu')({target: li, preventDefault() {}});
            return JSON.parse(ipc.filter(call => call.command === 'show_context_menu').at(-1).args.items);
        },
        openPreview(ref, actions, selectedText = '', isCurrent = () => true) {
            const li = {isConnected: true,
                dataset: {source: 'clipboard', clipboardKind: ref.kind, clipboardId: ref.id, actions: JSON.stringify(actions)},
                querySelector: () => null};
            const target = {previewContext: {anchor: li, selectedText, isCurrent}, closest: () => null};
            events.get('contextmenu')({target, preventDefault() {}});
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
        const copyIndex = menu.findIndex(item => item.label === 'menu.copy');
        assert.equal(menu[copyIndex].shortcut, 'Ctrl+C');
        h.events.get('action')({payload: {actionId: copyIndex, menuId: menu[copyIndex].menuId, generation: menu[copyIndex].generation}});
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

const textActions = [
    {kind: 'copy', hint: 'menu.copy', hitId: 'preview-text'},
    {kind: 'run', runId: 'edit_text_item', hint: 'menu.edit', runArg: {originRef: 'preview-text'}},
    {kind: 'run', runId: 'pin_text_item', hint: 'menu.sticky', runArg: {originRef: 'preview-text'}},
];
const textRef = {kind: 'text', id: 'preview-text'};
const imageRef = {kind: 'image', id: 'preview-image'};
const imageActions = [{kind: 'run', runId: 'copy_clipboard_image', hint: 'menu.copy', runArg: imageRef.id}];
const choose = (h, menu, label) => {
    const index = menu.findIndex(item => item.label === label);
    assert.ok(index >= 0, 'menu contains ' + label);
    h.events.get('action')({payload: {actionId: index, menuId: menu[index].menuId, generation: menu[index].generation}});
};

test('文本预览复用单项菜单，选区只覆盖复制且不接管列表多选', async () => {
    const copied = [];
    const h = await setup([{kind: 'text', id: 'other'}, imageRef], {copyToClipboard: async text => copied.push(text)});
    const menu = h.openPreview(textRef, textActions, '所选片段\n');
    // 0.26.10：预览右键按所属单项处理——顶部注入「正在识别操作」智能组占位
    assert.deepEqual(menu.filter(item => !item.separator).map(item => item.label),
        ['menu.smart_loading', 'menu.copySelection', 'menu.edit', 'menu.sticky', 'menu.delete']);
    assert.equal(menu[0].disabled, true, 'loading 项不可点击');
    assert.equal(menu.find(item => item.label === 'menu.copySelection').shortcut, 'Ctrl+C');
    assert.equal(h.prepared.length, 0, 'preview does not mutate the list selection');
    h.holds[0].selectedText = 'changed selection';
    choose(h, menu, 'menu.copySelection');
    await Promise.resolve();
    assert.deepEqual(copied, ['所选片段\n'], 'copy uses the original selection snapshot');
    assert.equal(h.calls.length, 0, 'does not activate or copy the whole entry');
    for (const label of ['menu.edit', 'menu.sticky', 'menu.delete']) {
        const next = h.openPreview(textRef, textActions, '所选片段');
        choose(h, next, label);
        const call = h.calls.at(-1);
        if (label === 'menu.delete') {
            assert.equal(call.id, 'delete');
            assert.deepEqual(JSON.parse(JSON.stringify(call.task.refs)), [textRef]);
        } else {
            assert.equal(call.id, 'activate');
            assert.equal(call.data.actions[0].runArg.originRef, textRef.id, 'edit/sticky still target the complete entry');
        }
    }
    const full = h.openPreview(textRef, textActions);
    choose(h, full, 'menu.copy');
    assert.equal(h.calls.at(-1).data.actions[0].hitId, textRef.id, 'no selection retains full-entry copy');
    h.clipboardMode.isActive = () => false;
    assert.equal(h.openPreview(textRef, textActions, 'selection')[0].shortcut, 'Ctrl+C',
        'preview text selection retains native copy in ordinary search');
});

test('图片预览复制、钉图、编辑与删除均使用原图单项身份', async () => {
    const edited = [];
    const h = await setup([textRef, {kind: 'image', id: 'other'}], {
        openImageEditorFromHistory: async id => edited.push(id),
    });
    for (const label of ['menu.copy', 'menu.pin', 'menu.editImage', 'menu.delete']) {
        const menu = h.openPreview(imageRef, imageActions);
        assert.deepEqual(menu.filter(item => !item.separator).map(item => item.label),
            ['menu.copy', 'menu.pin', 'menu.editImage', 'menu.delete']);
        choose(h, menu, label);
        if (label === 'menu.copy') assert.equal(h.calls.at(-1).data.actions[0].runArg, imageRef.id);
        else if (label === 'menu.editImage') assert.deepEqual(edited, [imageRef.id]);
        else assert.deepEqual(JSON.parse(JSON.stringify(h.calls.at(-1).task.refs)), [imageRef]);
        await Promise.resolve();
    }
    assert.equal(h.prepared.length, 0);
});

test('预览菜单取消、失败与过期回调正确释放对应会话', async () => {
    const h = await setup([textRef, imageRef]);
    const old = h.openPreview(textRef, textActions);
    const latest = h.openPreview(imageRef, imageActions);
    assert.equal(h.releases.length, 1);
    h.events.get('closed')({payload: {menuId: old[0].menuId}});
    assert.equal(h.releases.length, 1, 'late dismissal must not release the new preview');
    h.events.get('closed')({payload: {menuId: latest[0].menuId}});
    assert.equal(h.releases.length, 2);
    choose(h, latest, 'menu.delete');
    assert.equal(h.calls.length, 0, 'dismissed menus cannot execute stale actions');

    let current = true;
    const stale = h.openPreview(textRef, textActions, '', () => current);
    current = false;
    choose(h, stale, 'menu.delete');
    assert.equal(h.calls.length, 0, 'a cleared or replaced preview cannot execute its menu');

    const failed = await setup([textRef], {
        invoke: async command => {if (command === 'show_context_menu') throw new Error('test popup failure');},
        console: {error() {}},
    });
    failed.events.get('contextmenu')({preventDefault() {}, target: {
        closest: () => null, previewContext: {anchor: {
            dataset: {clipboardKind: 'text', clipboardId: textRef.id, actions: JSON.stringify(textActions)},
            querySelector: () => null,
        }, selectedText: '', isCurrent: () => true},
    }});
    await Promise.resolve(); await Promise.resolve();
    assert.equal(failed.releases.length, 1, 'popup failure must not leave the hover permanently held');
});

test('菜单存在时 Esc 只关闭菜单并释放预览', async () => {
    const h = await setup([textRef]);
    h.openPreview(textRef, textActions);
    let stopped = false, prevented = false;
    h.events.get('keydown')({key: 'Escape', preventDefault() {prevented = true;}, stopPropagation() {stopped = true;}});
    assert.equal(stopped && prevented, true);
    assert.equal(h.releases.length, 1);
    assert.equal(h.calls.length, 0);
});
