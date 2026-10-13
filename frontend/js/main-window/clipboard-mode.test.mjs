import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as selection from './clipboard-selection.js';
import {parse as parseColor} from '../shared/color.js';

// 执行实际状态机，替换 DOM/IPC 边界；可手动推进防抖和任意顺序完成请求。
const source = readFileSync(new URL('./clipboard-mode.js', import.meta.url), 'utf8')
    .replace(/^import[\s\S]*?from\s+["'][^"']+["'];\s*/gm, '')
    .replace(/^export /gm, '');
const deferred = () => {
    let resolve, reject;
    const promise = new Promise((yes, no) => {resolve = yes; reject = no;});
    return {promise, resolve, reject};
};
const flush = async () => {for (let i = 0; i < 5; i++) await Promise.resolve();};

function setup() {
    selection._resetForTest();
    const timers = new Map(), requests = [], renders = [], hits = [], pastes = [], deletes = [], editors = [], stickies = [], copies = [], removed = [], appends = [];
    let timerId = 0, feedback = null, hiddenWindows = 0, nearTail = false, hasItems = true, loadedItems = [];
    const noop = () => {};
    const queryEl = {value: '', placeholder: '', focus: noop};
    const context = vm.createContext({
        queryEl, resultsEl: {querySelectorAll: () => []}, selection, parseColor,
        results: {
            setClipboardMode: noop, clear: noop,
            render: entries => renders.push(entries), refreshStatusbar: noop,
            // 0.25.29 流式/局部移除边界
            removeItems: keys => removed.push(keys),
            getItems: () => loadedItems.slice(),
            appendItems: (items, seq) => appends.push({items, seq}),
            nearTail: () => nearTail,
            hasItems: () => hasItems,
            getLoadedCount: () => loadedItems.length,
        },
        ghost: {clear: noop}, suggestionBar: {clear: noop},
        statusbar: {clearOverride: noop, setOverride: noop},
        document: {body: {classList: {add: noop, remove: noop}}},
        dismissFeedback: () => {feedback = null;}, showFeedback: message => {feedback = message;},
        syncWindowSize: noop, resyncWindowSize: noop, t: key => key,
        setTimeout: callback => {timers.set(++timerId, callback); return timerId;},
        clearTimeout: id => {timers.delete(id);},
        searchClipboard: (query, seq, kind, cursor, batch) => {
            const request = {...deferred(), query, seq, kind, cursor, batch};
            requests.push(request);
            return request.promise;
        },
        getClipboardTextBatch: async keys => keys.map(key => ({text: `text:${key}`})),
        copyToClipboard: async text => {copies.push(text);},
        recordClipboardHit: async key => {hits.push(key);},
        pasteToInput: text => {const request = {...deferred(), text}; pastes.push(request); return request.promise;},
        deleteClipboardItemsBatch: keys => {const request = {...deferred(), keys}; deletes.push(request); return request.promise;},
        openContentEditor: request => {const request2 = {...deferred(), payload: request}; editors.push(request2); return request2.promise;},
        createStickyNote: async text => {stickies.push(text); return {id: `note_${stickies.length}`};},
        showStickyWindow: async () => {},
        hideWindow: () => {hiddenWindows++;},
        getClipboardText: async id => `text:${id}`,
        showActionError: () => {feedback = 'error';},
        console: {log: noop, error: noop}, Date,
    });
    vm.runInContext(source, context);
    context.enter();
    return {
        context, queryEl, requests, renders, pastes, hits, deletes, editors, stickies, copies, removed, appends,
        get hiddenWindows() {return hiddenWindows;},
        set nearTail(value) {nearTail = value;},
        set hasItems(value) {hasItems = value;},
        set loaded(value) {loadedItems = value;},
        input(value) {queryEl.value = value; context.handleInput(value);},
        tick() {const pending = [...timers.values()]; timers.clear(); pending.forEach(callback => callback());},
        get feedback() {return feedback;}, set feedback(value) {feedback = value;},
        get timerCount() {return timers.size;},
    };
}

{
    const h = setup();
    h.input('latest');
    h.context.setKind('text');
    assert.equal(h.requests.at(-1).query, 'latest');
    assert.equal(h.requests.at(-1).kind, 'text');
    assert.equal(h.timerCount, 0, '类型切换取消旧防抖');
    h.requests[0].resolve({entries: [{name: 'stale'}]});
    h.requests.at(-1).resolve({entries: [{name: 'latest'}]});
    await flush();
    assert.equal(h.renders.length, 1);
    assert.equal(h.renders[0][0].name, 'latest');
}
{
    const h = setup();
    h.input('new');
    h.requests[0].resolve({entries: [{name: 'old'}]});
    await flush();
    assert.equal(h.renders.length, 0, '防抖期间旧响应也不能回流');
    h.input('');
    h.tick();
    assert.equal(h.requests.at(-1).query, '', '退回旧 query 也要撤销待发的新 query');
}
{
    const h = setup();
    h.input('#ff0000');
    assert.equal(h.renders.at(-1)[0].source, 'color');
    h.context.setKind('image');
    assert.equal(h.requests.at(-1).kind, 'image');
    h.context.setKind('text');
    assert.equal(h.renders.at(-1)[0].source, 'color', '类型切换复用颜色字面量逻辑');
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    const pending = h.context.batchPaste();
    await flush();
    assert.equal(h.pastes[0].text, 'text:a\ntext:b');
    h.context.reset();
    assert.equal(h.feedback, null, '隐藏时收掉持久 loading');
    h.context.enter();
    selection.toggleSelection('new');
    h.feedback = 'new feedback';
    h.pastes[0].resolve();
    await pending;
    assert.deepEqual(selection.getSelectedKeys(), ['new']);
    assert.equal(h.feedback, 'new feedback');
    assert.deepEqual(h.hits, ['a', 'b'], '旧成功仍记录命中');
}
for (const fail of [false, true]) {
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchPaste();
    await flush();
    if (fail) h.pastes[0].reject(new Error('busy')); else h.pastes[0].resolve();
    await pending;
    assert.equal(selection.hasSelection(), fail, '当前会话成功清选，失败保留');
    assert.equal(h.feedback, fail ? 'error' : null);
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    const pending = h.context.batchDelete();
    await flush();
    assert.deepEqual(h.deletes[0].keys, ['a', 'b'], '批量删除携带全部选中 key');
    h.deletes[0].resolve();
    await pending;
    assert.equal(selection.hasSelection(), false, '删除成功后清空选择');
    // setup 的 context.enter() 空 query 首搜是 requests 基线（deferred 永挂）；
    // 0.25.29 删除走局部移除，不再发起新的重查请求
    assert.equal(h.requests.length, 1, '删除走局部移除，不再全量重查');
    assert.deepEqual(h.removed.at(-1), ['a', 'b'], '局部移除携带本次删除的 key');
    assert.equal(h.feedback, 'clipboard.deleted_count');
}
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchDelete();
    await flush();
    h.deletes[0].reject(new Error('db locked'));
    await pending;
    assert.equal(selection.hasSelection(), true, '删除失败保留选择');
    assert.equal(h.feedback, 'clipboard.delete_failed');
}
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchDelete();
    await flush();
    h.context.reset(); // 删除进行中窗口隐藏：清选择 + 收掉持久 loading
    selection.toggleSelection('new'); // 新会话的新选择
    h.feedback = 'new feedback';
    h.deletes[0].resolve();
    await pending;
    assert.deepEqual(selection.getSelectedKeys(), ['new'], '迟到的成功只清本次删除的 key');
    assert.equal(h.feedback, 'new feedback', '迟到的成功不覆盖新会话反馈');
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    await h.context.batchCopy();
    assert.deepEqual(h.copies, ['text:a\ntext:b'], '批量复制按点击顺序拼接写剪贴板（组合入库，删旧留新去重）');
    assert.deepEqual(h.hits, ['a', 'b'], '记录全部选中项命中');
    assert.equal(selection.hasSelection(), false, '复制成功后清空选择');
    // setup 的 context.enter() 空 query 首搜是 requests 基线（deferred 永挂）；
    // batchCopy 的 reloadList 在单测同 tick 内被 250ms 短窗跳过（实机由
    // awareness-updated → retrigger 链在窗外补刷），故仍只有基线一条
    assert.equal(h.requests.length, 1, '组合入库 + 刷新（单测受 250ms 窗，见注释）');
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    const pending = h.context.batchEdit();
    await flush();
    assert.equal(h.editors.length, 1, '合并编辑打开一次编辑器');
    assert.equal(h.editors[0].payload.body, 'text:a\ntext:b', '编辑器正文为点击顺序拼接');
    assert.equal(h.editors[0].payload.source.kind, 'clipboard_item');
    assert.equal(h.editors[0].payload.source.itemRef, null, '无原条目引用——保存即新建历史条目（原件保留）');
    h.editors[0].resolve();
    await pending;
    assert.equal(h.hiddenWindows, 1, '编辑器打开成功后主窗隐藏');
}
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchEdit();
    h.context.reset(); // 取数返回前窗口隐藏——旧会话请求失效
    await flush();
    await pending;
    assert.equal(h.editors.length, 0, 'epoch 失效不开编辑器');
    assert.equal(h.hiddenWindows, 0);
}
{
    const h = setup();
    selection.toggleSelection('a'); selection.toggleSelection('b');
    const pending = h.context.batchSticky();
    await flush();
    assert.deepEqual(h.stickies, ['text:a\ntext:b'], '便签内容为点击顺序拼接');
    await pending;
    assert.equal(h.hiddenWindows, 1, '便签打开后主窗隐藏');
}

// ── 0.25.29 流式加载状态机 ─────────────────────────────────────────────────
// 真实链路：doSearch 设置游标后自己调 maybeLoadMore（修复前 render 阶段触发时
// 游标未就绪被跳过、数据只有一批时滚轮死锁——本组用例不显式调 onPageChanged，
// 锁死「resolve 首批即应自动触发追加」的顺序契约）。

{
    const h = setup();
    // 首批带 has_more → resolve 后自动触发 loadMore，携带游标追加
    h.nearTail = true;
    h.requests[0].resolve({
        entries: [{name: 'first'}],
        has_more: true,
        next_cursor: {created_at: 100, id: 'clip_1'},
    });
    await flush();
    assert.equal(h.requests.length, 2, 'resolve 首批后自动触发追加（游标就绪顺序契约）');
    assert.deepEqual(h.requests[1].cursor, {created_at: 100, id: 'clip_1'}, '追加携带上一批游标');
    h.requests[1].resolve({entries: [{name: 'second'}], has_more: false, next_cursor: null});
    await flush();
    assert.equal(h.appends.length, 1, '追加批走 appendItems 专用路径');
    assert.equal(h.appends[0].items[0].name, 'second');
    assert.equal(h.requests.length, 2, 'has_more=false 停止继续拉取');
}
{
    const h = setup();
    // 首批 has_more=false：即使 nearTail 也不追加
    h.nearTail = true;
    h.requests[0].resolve({entries: [{name: 'only'}], has_more: false});
    await flush();
    assert.equal(h.requests.length, 1, '无更多数据不追加');
}
{
    const h = setup();
    // 竞态：loadMore 在途时 query 变化（doSearch 重置游标）→ 迟到追加丢弃
    h.nearTail = true;
    h.requests[0].resolve({
        entries: [{name: 'first'}],
        has_more: true,
        next_cursor: {created_at: 100, id: 'clip_1'},
    });
    await flush();
    h.input('filter'); // 触发新 doSearch（seq++ + 游标重置）
    h.tick();
    h.requests[1].resolve({entries: [{name: 'stale batch'}], has_more: false});
    await flush();
    assert.equal(h.appends.length, 0, '旧会话的追加响应被丢弃');
}
{
    const h = setup();
    // 竞态：loadMore 在途时退出模式 → 追加丢弃
    h.nearTail = true;
    h.requests[0].resolve({
        entries: [{name: 'first'}],
        has_more: true,
        next_cursor: {created_at: 100, id: 'clip_1'},
    });
    await flush();
    h.context.exit();
    h.requests[1].resolve({entries: [{name: 'stale'}], has_more: false});
    await flush();
    assert.equal(h.appends.length, 0, '退出模式后的追加响应被丢弃');
}
{
    const h = setup();
    // nearTail=false：翻页未到末页附近不预取
    h.nearTail = false;
    h.requests[0].resolve({
        entries: [{name: 'first'}],
        has_more: true,
        next_cursor: {created_at: 100, id: 'clip_1'},
    });
    await flush();
    assert.equal(h.requests.length, 1, '距底充足不预取');
}
// ── 0.25.29 Ctrl+A 全选实时提取（流式追加覆盖） ────────────────────────────
{
    // 实测回归：流式追加后 Ctrl+A 应选中全部已加载文本项（曾只选首批快照，
    // 首批 14 条含浮动数量的图片项 → "有时 11 条有时 12 条"）
    const h = setup();
    const text = (hitId) => ({name: hitId, source: 'clipboard', is_image: false, actions: [{hitId}]});
    const image = (id) => ({name: id, source: 'clipboard', is_image: true, lnk_path: id, actions: []});
    h.loaded = [text('a'), image('img_1'), text('b'), text('c'), image('img_2')];
    h.context.handleKeydown({ctrlKey: true, key: 'a', isComposing: false, preventDefault: () => {}});
    assert.deepEqual(selection.getSelectedKeys(), ['a', 'b', 'c'], 'Ctrl+A 实时选中全部已加载文本项（图片跳过）');
    // 非空 query 保留输入框原生全选
    h.input('filter');
    assert.equal(
        h.context.handleKeydown({ctrlKey: true, key: 'a', isComposing: false, preventDefault: () => {}}),
        false,
        '非空 query 不接管 Ctrl+A'
    );
}
console.log('剪贴板模式：防抖、类型切换、响应乱序、批量上屏、批量删除、合并编辑/便签会话隔离、流式加载与 Ctrl+A 实时全选通过');
