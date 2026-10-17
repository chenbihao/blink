import {beforeEach, test} from 'node:test';
import assert from 'node:assert/strict';
import * as selection from './clipboard-selection.js';
import {entryRef, refKey, refFromKey, selectionActions} from './clipboard-entry.js';
import {createClipboardBatch} from './clipboard-batch.js';

const text = id => ({kind: 'text', id});
const image = id => ({kind: 'image', id});
const keys = refs => refs.map(refKey);
const deferred = () => {
    let resolve, reject;
    const promise = new Promise((yes, no) => {resolve = yes; reject = no;});
    return {promise, resolve, reject};
};
beforeEach(() => { selection._resetForTest(); selection.onEnterMode(); });

test('同 ID 的不同类型独立选择，ID 中含分隔符也不会冲突', () => {
    selection.selectAll(keys([text('same:[]'), image('same:[]')]));
    assert.equal(selection.selectedCount(), 2);
    assert.deepEqual(selection.getSelectedKeys().map(refFromKey), [text('same:[]'), image('same:[]')]);
    assert.equal(entryRef({actions: [{hitId: 'x'}]}), null, '不再从执行动作猜身份');
});
test('Shift 范围跨类型、跨页，锚点固定，反向缩回不累积旧区间', () => {
    const order = keys([text('a'), image('b'), text('c'), image('d'), text('e')]);
    selection.selectSingle(order[1]);
    selection.selectRange(order, order[4]);
    assert.deepEqual(selection.getSelectedKeys(), order.slice(1));
    selection.selectRange(order, order[2]);
    assert.deepEqual(selection.getSelectedKeys(), order.slice(1, 3));
    selection.selectRange(order, order[0]);
    assert.deepEqual(selection.getSelectedKeys(), order.slice(0, 2));
});
test('Ctrl+Shift 保留原选择，连续缩小追加区间不残留', () => {
    const order = keys([text('a'), image('b'), text('c'), image('d')]);
    selection.toggleSelection(order[0]); selection.toggleSelection(order[2]);
    selection.selectRange(order, order[3], true);
    assert.deepEqual(selection.getSelectedKeys(), [order[0], order[2], order[3]]);
    selection.selectRange(order, order[1], true);
    assert.deepEqual(selection.getSelectedKeys(), [order[0], order[2], order[1]]);
});
test('追加保留锚点，删除锚点和查询变化清理区间状态', () => {
    const order = keys([text('a'), image('b'), text('c')]);
    selection.selectSingle(order[0]); selection.reconcileAfterReorder(order);
    selection.selectRange(order, order[2]);
    assert.deepEqual(selection.getSelectedKeys(), order);
    selection.reconcileAfterReorder(order.slice(1));
    assert.equal(selection.getAnchorKey(), null);
    selection.onQueryChanged();
    assert.equal(selection.hasSelection(), false);
    assert.equal(selection.getAnchorKey(), null);
});
test('混选菜单只针对对应子集，单类型不重复列出删除动作', () => {
    const refs = [text('a'), image('b'), text('c'), image('d')];
    const actions = selectionActions(refs);
    assert.deepEqual(actions.find(a => a.id === 'copy').refs, [text('a'), text('c')]);
    assert.equal(actions.find(a => a.id === 'copy').shortcut, '');
    assert.deepEqual(actions.find(a => a.id === 'pin').refs, [image('b'), image('d')]);
    assert.deepEqual(actions.filter(a => a.id === 'delete').map(a => a.scope), ['text', 'image', 'all']);
    assert.equal(selectionActions([image('b'), image('d')]).filter(a => a.id === 'delete').length, 1);
    assert.equal(selectionActions([{kind: 'audio', id: 'future'}]).length, 0);
    assert.equal(selectionActions([{kind: 'toString', id: 'unknown'}]).length, 0);
});

function harness(refs) {
    selection.selectAll(keys(refs));
    let visible = true;
    const calls = {copies: [], requests: [], removed: [], feedback: []};
    const snapshot = () => ({...selection.capture(), refs: selection.getSelectedKeys().map(refFromKey)});
    const api = {
        getClipboardTextBatch: async ids => ids.slice().reverse().map(id => ({id, text: id})),
        copyToClipboard: async body => calls.copies.push(body),
        pasteToInput: async () => {}, openContentEditor: async () => {},
        createStickyNote: async () => ({id: 'note'}), showStickyWindow: async () => {},
        hideWindow: () => { visible = false; }, recordClipboardHit: async () => {},
        manageClipboardEntries: (operation, entries) => {
            const request = {...deferred(), operation, entries}; calls.requests.push(request); return request.promise;
        },
    };
    const batch = createClipboardBatch({selection, snapshot, active: () => visible,
        refresh() {}, reload() {}, remove: refs => calls.removed.push(refs), api,
        feedback: (message, options) => calls.feedback.push({message, options}), dismiss() {},
        t: (key, params) => ({key, params})});
    return {calls, batch, snapshot, api, hide: () => {visible = false; selection.onExitMode();}};
}
test('混选默认复制不静默丢掉图片，显式复制文本子集保留图片选择', async () => {
    const h = harness([text('a'), image('b'), text('c')]);
    await h.batch.textOperation('copy');
    assert.equal(h.calls.copies.length, 0);
    const task = h.snapshot(); task.refs = [text('a'), text('c')]; task.keys = keys(task.refs);
    await h.batch.textOperation('copy', task);
    assert.deepEqual(h.calls.copies, ['a\nc'], '响应乱序仍按用户选择顺序组合');
    assert.deepEqual(selection.getSelectedKeys(), keys([image('b')]));
});
test('批量删除部分失败只移除成功项，失败项保留供重试', async () => {
    const h = harness([text('a'), image('b')]);
    const pending = h.batch.resourceOperation('delete');
    h.calls.requests[0].resolve([{entry: text('a'), success: true}, {entry: image('b'), success: false, error: 'locked'}]);
    await pending;
    assert.deepEqual(h.calls.removed, [[text('a')]]);
    assert.deepEqual(selection.getSelectedKeys(), keys([image('b')]));
    assert.deepEqual(h.calls.feedback.at(-1).message.params, {success: 1, failed: 1});
});
test('只删除图片子集不清除文本选择', async () => {
    const h = harness([text('a'), image('b')]);
    const task = h.snapshot(); task.refs = [image('b')]; task.keys = keys(task.refs);
    const pending = h.batch.resourceOperation('delete', task);
    h.calls.requests[0].resolve([{entry: image('b'), success: true}]); await pending;
    assert.deepEqual(selection.getSelectedKeys(), keys([text('a')]));
});
test('复制等待期间的新选择不会被旧成功清理', async () => {
    const h = harness([text('a')]); const copy = deferred();
    h.api.copyToClipboard = () => copy.promise;
    const pending = h.batch.textOperation('copy');
    await Promise.resolve(); await Promise.resolve();
    selection.toggleSelection(refKey(image('new'))); copy.resolve(); await pending;
    assert.deepEqual(selection.getSelectedKeys(), keys([text('a'), image('new')]));
});
test('菜单旧选择快照在执行前失效，不发出删除请求', async () => {
    const h = harness([text('a')]); const task = h.snapshot();
    selection.selectSingle(refKey(image('b')));
    await h.batch.resourceOperation('delete', task);
    assert.equal(h.calls.requests.length, 0);
});
test('钉图防重入，隐藏后旧回流不污染新会话', async () => {
    const h = harness([image('a'), image('b')]);
    const pending = h.batch.resourceOperation('pin');
    await h.batch.resourceOperation('pin');
    assert.equal(h.calls.requests.length, 1);
    h.hide(); selection.onEnterMode(); selection.selectSingle(refKey(text('new')));
    const feedbackCount = h.calls.feedback.length;
    h.calls.requests[0].resolve([{entry: image('a'), success: true}, {entry: image('b'), success: true}]); await pending;
    assert.deepEqual(selection.getSelectedKeys(), keys([text('new')]));
    assert.equal(h.calls.feedback.length, feedbackCount);
});

// ── 0.26.9 文件条目操作矩阵 ────────────────────────────────────────────
const file = id => ({kind: 'file', id});
test('文件条目支持复制路径与删除，不提供文本/图片专属操作', () => {
    const actions = selectionActions([file('f1'), text('a'), image('i1')]);
    assert.deepEqual(actions.map(action => action.id),
        ['copy', 'copy_paths', 'paste', 'edit', 'sticky', 'pin', 'delete', 'delete', 'delete', 'delete'],
        'copy/paste/edit/sticky 只含文本子集，copy_paths 只含文件子集，删除按类型分组 + 全部');
    const copyPaths = actions.find(action => action.id === 'copy_paths');
    assert.deepEqual(copyPaths.refs, [file('f1')]);
    assert.equal(copyPaths.shortcut, '', '混选不标注快捷键');
    // 纯文件选择标注 Ctrl+C
    const pure = selectionActions([file('f1'), file('f2')]);
    assert.equal(pure.find(action => action.id === 'copy_paths').shortcut, 'Ctrl+C');
    // 文件不可进入文本执行路径
    assert.equal(selectionActions([file('f1')]).some(action => ['copy', 'paste', 'edit', 'sticky', 'pin'].includes(action.id)), false);
});
test('文件条目与文本同 ID 独立身份，删除分组含 file', () => {
    const actions = selectionActions([file('same'), text('same'), image('same')]);
    const scopes = actions.filter(action => action.id === 'delete').map(action => action.scope);
    assert.deepEqual(scopes.sort(), ['all', 'file', 'image', 'text']);
});
