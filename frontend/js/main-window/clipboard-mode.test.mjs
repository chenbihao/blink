import assert from 'node:assert/strict';
import {readFileSync} from 'node:fs';
import vm from 'node:vm';
import * as selectionState from './clipboard-selection.js';
import * as entryModel from './clipboard-entry.js';
import {createClipboardBatch} from './clipboard-batch.js';
const selection = {...selectionState,
    toggleSelection: id => selectionState.toggleSelection(entryModel.refKey({kind: 'text', id})),
    getSelectedKeys: () => selectionState.getSelectedKeys().map(key => entryModel.refFromKey(key)?.id ?? key),
};
const fixtureEntry = item => {
    if (item.clipboard_ref) return item;
    const id = item.is_image ? (item.lnk_path || item.actions?.[0]?.hitId) : item.actions?.[0]?.hitId;
    return id ? {...item, clipboard_ref: {kind: item.is_image ? 'image' : 'text', id}} : item;
};
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
    let timerId = 0, feedback = null, hiddenWindows = 0, nearTail = false, hasItems = true, loadedItems = [], activeItem = null, lis = [];
    const activations = [];
    const noop = () => {};
    const queryEl = {value: '', placeholder: '', focus: noop};
    const context = vm.createContext({
        listNavigation: {reset: () => {}},
        queryEl, resultsEl: {querySelectorAll: () => lis}, selection: selectionState, parseColor,
        ...entryModel,
        createClipboardBatch: deps => createClipboardBatch({...deps,
            api: new Proxy(deps.api, {get: (target, key) => context[key] ?? target[key]})}),
        results: {
            setClipboardMode: noop, clear: noop,
            getPage: () => 0, getSelectedIndex: () => 0, restorePosition: noop,
            render: entries => renders.push(entries), refreshStatusbar: noop,
            // 0.25.29 流式/局部移除边界
            removeItems: refs => removed.push(refs.map(ref => ref.id)),
            getItems: () => loadedItems.slice(), getActive: () => activeItem,
            setActiveByEl: li => {activeItem = {clipboardRef: entryModel.refFromLi(li), actions: JSON.parse(li.dataset.actions || '[]')};},
            appendItems: (items, seq) => appends.push({items, seq}),
            nearTail: () => nearTail,
            hasItems: () => hasItems,
            getLoadedCount: () => loadedItems.length,
        },
        ghost: {clear: noop}, suggestionBar: {clear: noop},
        statusbar: {clearOverride: noop, setOverride: noop},
        document: {body: {classList: {add: noop, remove: noop}}},
        dismissFeedback: () => {feedback = null;}, showFeedback: message => {feedback = message;},
        syncWindowSize: noop, resyncWindowSize: noop,
        setClipboardLayout: noop, markClipboardLayoutReady: noop, t: key => key,
        setTimeout: callback => {timers.set(++timerId, callback); return timerId;},
        clearTimeout: id => {timers.delete(id);},
        searchClipboard: (query, seq, kind, cursor, batch) => {
            const request = {...deferred(), query, seq, kind, cursor, batch};
            const resolve = request.resolve;
            request.resolve = value => resolve({...value, entries: value.entries?.map(fixtureEntry)});
            requests.push(request);
            return request.promise;
        },
        getClipboardTextBatch: async keys => keys.map(key => ({id: key, text: `text:${key}`})),
        copyToClipboard: async text => {copies.push(text);},
        recordClipboardHit: async key => {hits.push(key);},
        pasteToInput: text => {const request = {...deferred(), text}; pastes.push(request); return request.promise;},
        manageClipboardEntries: (operation, refs) => {
            const request = {...deferred(), keys: refs.map(ref => ref.id)};
            const resolve = request.resolve;
            request.resolve = value => resolve(value ?? refs.map(entry => ({entry, success: true})));
            deletes.push(request); return request.promise;
        },
        copyClipboardImage: async () => {},
        activateItem: item => activations.push(item),
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
        context, queryEl, requests, renders, pastes, hits, deletes, editors, stickies, copies, removed, appends, activations,
        set lis(value) {lis = value;},
        set active(value) {activeItem = value;},
        get active() {return activeItem;},
        get hiddenWindows() {return hiddenWindows;},
        set nearTail(value) {nearTail = value;},
        set hasItems(value) {hasItems = value;},
        set loaded(value) {loadedItems = value.map(fixtureEntry);},
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
    assert.equal(h.feedback, fail ? 'clipboard.operation_failed' : null);
}
for (const fail of [false, true]) {
    const h = setup();
    const ref = {kind: 'image', id: 'older-image'};
    selectionState.selectSingle(entryModel.refKey(ref));
    h.context.copyClipboardImage = async id => {
        assert.equal(id, ref.id);
        if (fail) throw new Error('clipboard busy');
    };
    await h.context.batchCopy();
    assert.equal(h.requests.length, fail ? 1 : 2, '图片复制成功即刷新置顶，失败不重排，250ms 内也不漏刷新');
    assert.equal(selectionState.hasSelection(), fail);
    if (!fail) {
        h.requests[1].resolve({entries: [{is_image: true, lnk_path: 'promoted-image', name: 'promoted'}]});
        await flush();
        assert.equal(h.renders.at(-1)[0].name, 'promoted', '刷新呈现置顶后的图片');
        h.requests[0].resolve({entries: [{name: 'stale'}]});
        await flush();
        assert.equal(h.renders.length, 1, '置顶刷新后旧搜索响应不能覆盖新顺序');
    }
}
{
    const h = setup(), copied = deferred();
    selectionState.selectSingle(entryModel.refKey({kind: 'image', id: 'old'}));
    h.context.copyClipboardImage = () => copied.promise;
    const pending = h.context.batchCopy();
    h.context.reset(); h.context.enter();
    selection.toggleSelection('new');
    const requests = h.requests.length;
    copied.resolve(); await pending;
    assert.equal(h.requests.length, requests, '旧图片复制回流不刷新新会话');
    assert.deepEqual(selection.getSelectedKeys(), ['new']);
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
    assert.equal(h.feedback, 'clipboard.delete_result');
}
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchDelete();
    await flush();
    h.deletes[0].reject(new Error('db locked'));
    await pending;
    assert.equal(selection.hasSelection(), true, '删除失败保留选择');
    assert.equal(h.feedback, 'clipboard.operation_failed');
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
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchEdit();
    await flush();
    h.context.reset(); h.context.enter();
    h.editors[0].resolve(); await pending;
    assert.equal(h.hiddenWindows, 0, '旧编辑器打开完成不能隐藏重开的主窗');
}
{
    const h = setup();
    selection.toggleSelection('a');
    const pending = h.context.batchDelete();
    h.context.reset(); h.context.enter();
    selection.toggleSelection('a');
    h.deletes[0].resolve(); await pending;
    assert.deepEqual(selection.getSelectedKeys(), ['a'], '旧删除不能清理新会话同 id 的选择');
    assert.equal(h.removed.length, 0);
}
{
    const h = setup();
    const text = deferred();
    h.context.getClipboardText = () => text.promise;
    const pending = h.context.activateByData({clipboardRef: {kind: 'text', id: 'a'}, actions: [{kind: 'copy', hitId: 'a'}]});
    h.context.reset(); h.context.enter();
    text.resolve('old text'); await pending;
    assert.equal(h.pastes.length, 0, '旧单条全文取回不能向新会话目标发起粘贴');
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
for (const oldFails of [false, true]) {
    const h = setup();
    h.nearTail = true;
    h.requests[0].resolve({entries: [{}], has_more: true, next_cursor: {created_at: 100, id: 'old'}});
    await flush();
    h.input('new'); h.tick();
    h.requests[2].resolve({entries: [{}], has_more: true, next_cursor: {created_at: 90, id: 'new'}});
    await flush();
    assert.equal(h.requests.length, 4, '旧追加未完成不阻塞新查询预取');
    if (oldFails) h.requests[1].reject(new Error('old failure'));
    else h.requests[1].resolve({entries: [{}], has_more: false});
    await flush();
    assert.equal(h.context.getStreamInfo().hasMore, true, '旧追加失败/成功不能关闭新流');
    h.context.onPageChanged();
    assert.equal(h.requests.length, 4, '旧 finally 不能解除新追加 single-flight');
    h.requests[3].resolve({entries: [{}], has_more: false});
    await flush();
    assert.equal(h.appends.length, 1);
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
    assert.deepEqual(selection.getSelectedKeys(), ['a', 'img_1', 'b', 'c', 'img_2'], 'Ctrl+A 选中全部已加载历史项（包含图片）');
    // 非空 query 保留输入框原生全选
    h.input('filter');
    assert.equal(
        h.context.handleKeydown({ctrlKey: true, key: 'a', isComposing: false, preventDefault: () => {}}),
        false,
        '非空 query 不接管 Ctrl+A'
    );
}
console.log('剪贴板模式：防抖、类型切换、响应乱序、批量上屏、批量删除、合并编辑/便签会话隔离、流式加载与 Ctrl+A 实时全选通过');

// 真实鼠标入口与 CSS 投影：图片、Ctrl 混选、跨页 Shift 与右键目标。
{
    const h = setup();
    const refs = [{kind: 'text', id: 'a'}, {kind: 'image', id: 'b'}, {kind: 'text', id: 'c'}, {kind: 'image', id: 'd'}];
    const makeLi = ref => ({dataset: {clipboardKind: ref.kind, clipboardId: ref.id, actions: '[]'},
        selected: false, classList: {toggle(name, selected) {this.owner.selected = selected;}},});
    const lis = refs.map(makeLi); lis.forEach(li => {li.classList.owner = li;});
    h.lis = lis; h.loaded = refs.map(clipboard_ref => ({clipboard_ref}));
    const event = (ctrlKey = false, shiftKey = false) => ({button: 0, ctrlKey, shiftKey, preventDefault() {}, stopPropagation() {}});
    assert.equal(h.context.handleMousedown(event(), lis[1]), true);
    assert.equal(lis[1].selected, true, '图片得到选择高亮');
    h.context.handleMousedown(event(true), lis[0]);
    assert.deepEqual(selectionState.getSelectedKeys(), refs.slice(0,2).reverse().map(entryModel.refKey));
    assert.deepEqual(h.active.clipboardRef, refs[0], 'Ctrl 选择同时移动活动项');
    h.context.handleMousedown(event(false, true), lis[3]);
    assert.deepEqual(selectionState.getSelectedKeys(), refs.map(entryModel.refKey), 'Shift 采用完整已加载顺序');
    h.context.handleMousedown(event(false, true), lis[1]);
    assert.deepEqual(selectionState.getSelectedKeys(), refs.slice(0,2).map(entryModel.refKey), 'Shift 缩回');
    h.context.prepareContextMenu(lis[0]);
    assert.equal(selectionState.selectedCount(), 2, '右键已选项保留组');
    h.context.prepareContextMenu(lis[3]);
    assert.deepEqual(selectionState.getSelectedKeys(), [entryModel.refKey(refs[3])], '右键未选项改为单选');
    h.active = {clipboardRef: refs[1]};
    h.context.onNavigate({shiftKey: true});
    assert.deepEqual(selectionState.getSelectedKeys(), refs.slice(1).map(entryModel.refKey), 'Shift 键盘导航与鼠标共用区间逻辑');
    selectionState.selectSingle(entryModel.refKey(refs[0]));
    selectionState.toggleSelection(entryModel.refKey(refs[2]));
    h.active = {clipboardRef: refs[3]};
    h.context.onNavigate({ctrlKey: true, shiftKey: true});
    assert.deepEqual(selectionState.getSelectedKeys(), [refs[0], refs[2], refs[3]].map(entryModel.refKey), 'Ctrl+Shift 键盘导航追加区间');
}
{
    const h = setup();
    const ref = {kind: 'image', id: 'only_image'};
    h.loaded = [{clipboard_ref: ref, is_image: true, actions: [{kind: 'run', runId: 'copy_clipboard_image'}]}];
    selectionState.selectSingle(entryModel.refKey(ref));
    h.context.activateSelection();
    assert.equal(h.activations.length, 1, '单图片 Enter 保留原图片激活路径');
    selectionState.toggleSelection(entryModel.refKey({kind: 'text', id: 'also_text'}));
    await h.context.activateSelection();
    assert.equal(h.pastes.length, 0, '混选 Enter 不静默上屏文本子集');
}
